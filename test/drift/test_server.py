#!/usr/bin/env python3
"""Sendspin source with a deterministic, deliberately skewed server clock."""

import asyncio
import json
import os
import struct
import time

import websockets


SAMPLE_RATE = 48_000
CHANNELS = 2
BIT_DEPTH = 24
BYTES_PER_SAMPLE = 3
FRAME_BYTES = CHANNELS * BYTES_PER_SAMPLE
CHUNK_FRAMES = 480
CHUNK_DURATION_SECONDS = CHUNK_FRAMES / SAMPLE_RATE
PORT = 8927

DRIFT_PPM = float(os.environ.get("DRIFT_PPM", "-250"))
DURATION_SECONDS = int(os.environ.get("AUDIO_DURATION_SECONDS", "90"))
LEAD_US = int(os.environ.get("AUDIO_LEAD_US", "500000"))
TIME_SYNC_SAMPLES = int(os.environ.get("TIME_SYNC_SAMPLES", "5"))
START_SIGNAL_PATH = os.environ.get(
    "START_SIGNAL_PATH",
    "/shared/start_audio",
)


class SkewClock:
    """Monotonic server clock whose rate differs from real time by `ppm`."""

    def __init__(self, ppm):
        self.rate = 1.0 + ppm / 1_000_000.0
        self.real_start_ns = time.monotonic_ns()
        self.server_start_us = time.time_ns() // 1_000

    def now_us(self):
        elapsed_us = (time.monotonic_ns() - self.real_start_ns) / 1_000
        return self.server_start_us + int(elapsed_us * self.rate)


def xorshift32(state):
    state &= 0xFFFFFFFF
    state ^= (state << 13) & 0xFFFFFFFF
    state ^= state >> 17
    state ^= (state << 5) & 0xFFFFFFFF
    return state & 0xFFFFFFFF


def signed_24(state):
    sample = (state >> 8) & 0xFFFFFF
    return sample - 0x1000000 if sample & 0x800000 else sample


def generate_signal(total_frames):
    """Return wire-format s24 PCM and write its capture-domain i32 reference."""
    pcm = bytearray(total_frames * FRAME_BYTES)
    reference_pcm = bytearray(total_frames * CHANNELS * 4)
    capture_frame = struct.Struct("<ii")
    left_state = 0x13579BDF
    right_state = 0x2468ACE1

    for frame in range(total_frames):
        left_state = xorshift32(left_state)
        right_state = xorshift32(right_state)
        left = signed_24(left_state)
        right = signed_24(right_state)

        offset = frame * FRAME_BYTES
        pcm[offset:offset + 3] = left.to_bytes(3, "little", signed=True)
        pcm[offset + 3:offset + 6] = right.to_bytes(3, "little", signed=True)
        capture_frame.pack_into(
            reference_pcm,
            frame * capture_frame.size,
            left << 8,
            right << 8,
        )

    with open("/shared/reference_capture.raw", "wb") as reference:
        reference.write(reference_pcm)

    return bytes(pcm)


def text_message(message_type, payload=None):
    return json.dumps({"type": message_type, "payload": payload or {}})


def audio_message(timestamp_us, pcm):
    return struct.pack(">Bq", 4, timestamp_us) + pcm


async def handle_client(websocket, pcm):
    print("[server] client connected", flush=True)
    hello = json.loads(await websocket.recv())
    if hello.get("type") != "client/hello":
        raise RuntimeError(f"expected client/hello, got {hello.get('type')}")

    send_lock = asyncio.Lock()
    clock = SkewClock(DRIFT_PPM)
    sync_ready = asyncio.Event()
    sync_count = 0

    async def send(message):
        async with send_lock:
            await websocket.send(message)

    await send(text_message("server/hello", {
        "server_id": "drift-test",
        "name": "Drift Test Server",
        "version": 1,
        "active_roles": ["player@v1"],
        "connection_reason": "playback",
    }))

    async def receive_client_messages():
        nonlocal sync_count
        async for raw in websocket:
            if not isinstance(raw, str):
                continue
            message = json.loads(raw)
            if message.get("type") != "client/time":
                continue

            server_received = clock.now_us()
            client_transmitted = message.get("payload", {}).get("client_transmitted")
            if client_transmitted is None:
                continue
            server_transmitted = clock.now_us()
            await send(text_message("server/time", {
                "client_transmitted": client_transmitted,
                "server_received": server_received,
                "server_transmitted": server_transmitted,
            }))
            sync_count += 1
            if sync_count >= TIME_SYNC_SAMPLES:
                sync_ready.set()

    receiver = asyncio.create_task(receive_client_messages())
    try:
        await asyncio.wait_for(sync_ready.wait(), timeout=10)
        print(
            f"[server] clock sync ready after {sync_count} exchanges; "
            f"rate={DRIFT_PPM:+.1f}ppm",
            flush=True,
        )
        print(f"[server] waiting for capture signal {START_SIGNAL_PATH}", flush=True)
        while not os.path.exists(START_SIGNAL_PATH):
            await asyncio.sleep(0.1)

        total_frames = len(pcm) // FRAME_BYTES

        await send(text_message("stream/start", {
            "player": {
                "codec": "pcm",
                "sample_rate": SAMPLE_RATE,
                "channels": CHANNELS,
                "bit_depth": BIT_DEPTH,
            }
        }))

        first_timestamp_us = clock.now_us() + LEAD_US
        real_start = asyncio.get_running_loop().time()
        frames_sent = 0
        while frames_sent < total_frames:
            end_frame = min(frames_sent + CHUNK_FRAMES, total_frames)
            start_byte = frames_sent * FRAME_BYTES
            end_byte = end_frame * FRAME_BYTES
            timestamp_us = first_timestamp_us + frames_sent * 1_000_000 // SAMPLE_RATE
            await send(audio_message(timestamp_us, pcm[start_byte:end_byte]))
            frames_sent = end_frame

            deadline = real_start + frames_sent / SAMPLE_RATE
            delay = deadline - asyncio.get_running_loop().time()
            if delay > 0:
                await asyncio.sleep(delay)

        print(f"[server] sent {frames_sent} frames", flush=True)
        await send(text_message("stream/end"))
        await asyncio.sleep(5)
    finally:
        receiver.cancel()
        await asyncio.gather(receiver, return_exceptions=True)


async def main():
    pcm = generate_signal(DURATION_SECONDS * SAMPLE_RATE)
    print(
        f"[server] generated {DURATION_SECONDS}s deterministic reference",
        flush=True,
    )
    print(
        f"[server] listening on ws://0.0.0.0:{PORT}/sendspin "
        f"with clock skew {DRIFT_PPM:+.1f}ppm",
        flush=True,
    )
    async with websockets.serve(
        lambda websocket: handle_client(websocket, pcm),
        "0.0.0.0",
        PORT,
    ):
        await asyncio.Future()


if __name__ == "__main__":
    asyncio.run(main())

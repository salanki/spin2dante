#!/usr/bin/env python3
"""Minimal Sendspin test source that emits deterministic 32-bit PCM."""

import asyncio
import contextlib
import json
import os
import struct
import time

import websockets

SAMPLE_RATE = 48000
CHANNELS = 2
BIT_DEPTH = 32
BYTES_PER_SAMPLE = BIT_DEPTH // 8
FRAME_SIZE = BYTES_PER_SAMPLE * CHANNELS
CHUNK_FRAMES = 480
PORT = 8927
STREAM_DURATION_SECS = 30
START_SIGNAL_PATH = "/shared/start_stream"

stream_served = False
SERVER_MONO_BASE_US = time.monotonic_ns() // 1_000


def xorshift32(state: int) -> int:
    state &= 0xFFFFFFFF
    state ^= (state << 13) & 0xFFFFFFFF
    state ^= (state >> 17) & 0xFFFFFFFF
    state ^= (state << 5) & 0xFFFFFFFF
    return state & 0xFFFFFFFF


def to_signed_32(state: int) -> int:
    sample = state & 0xFFFFFFFF
    if sample >= 0x80000000:
        sample -= 0x100000000
    return sample


def generate_reference_capture(path: str) -> None:
    left_state = 0x13579BDF
    right_state = 0x2468ACE1
    total_frames = STREAM_DURATION_SECS * SAMPLE_RATE

    with open(path, "wb") as raw_file:
        for _ in range(total_frames):
            left_state = xorshift32(left_state)
            right_state = xorshift32(right_state)
            raw_file.write(struct.pack("<i", to_signed_32(left_state)))
            raw_file.write(struct.pack("<i", to_signed_32(right_state)))


def make_text_msg(msg_type: str, payload: dict | None = None) -> str:
    obj = {"type": msg_type, "payload": payload or {}}
    return json.dumps(obj)


def make_audio_binary(timestamp_us: int, pcm_data: bytes) -> bytes:
    return struct.pack(">Bq", 4, timestamp_us) + pcm_data


def server_now_us() -> int:
    return (time.monotonic_ns() // 1_000) - SERVER_MONO_BASE_US


def generate_chunk(left_state: int, right_state: int, frames: int) -> tuple[bytes, int, int]:
    data = bytearray(frames * FRAME_SIZE)
    for frame in range(frames):
        left_state = xorshift32(left_state)
        right_state = xorshift32(right_state)
        offset = frame * FRAME_SIZE
        data[offset : offset + 4] = struct.pack("<i", to_signed_32(left_state))
        data[offset + 4 : offset + 8] = struct.pack("<i", to_signed_32(right_state))
    return bytes(data), left_state, right_state


async def send_audio(ws) -> None:
    left_state = 0x13579BDF
    right_state = 0x2468ACE1
    frames_total = SAMPLE_RATE * STREAM_DURATION_SECS
    frames_sent = 0
    ts = server_now_us() + 200_000

    while frames_sent < frames_total:
        chunk, left_state, right_state = generate_chunk(left_state, right_state, CHUNK_FRAMES)
        await ws.send(make_audio_binary(ts, chunk))
        frames_sent += CHUNK_FRAMES
        ts += int(CHUNK_FRAMES / SAMPLE_RATE * 1_000_000)
        await asyncio.sleep(CHUNK_FRAMES / SAMPLE_RATE)


async def wait_for_start_signal() -> None:
    print(f"[server] waiting for start signal at {START_SIGNAL_PATH}", flush=True)
    while not os.path.exists(START_SIGNAL_PATH):
        await asyncio.sleep(0.2)
    print("[server] start signal detected", flush=True)


async def handle_client_messages(ws, sync_ready: asyncio.Event) -> None:
    time_sync_count = 0
    async for raw in ws:
        if not isinstance(raw, str):
            continue
        msg = json.loads(raw)
        msg_type = msg.get("type")
        payload = msg.get("payload", {})
        if msg_type == "client/time":
            client_transmitted = payload.get("client_transmitted")
            server_received = server_now_us()
            time_sync_count += 1
            if time_sync_count == 1 or time_sync_count % 5 == 0:
                print(
                    f"[server] client/time #{time_sync_count}: client={client_transmitted} server={server_received}",
                    flush=True,
                )
            await ws.send(
                make_text_msg(
                    "server/time",
                    {
                        "client_transmitted": client_transmitted,
                        "server_received": server_received,
                        "server_transmitted": server_now_us(),
                    },
                )
            )
            if time_sync_count >= 5:
                sync_ready.set()
        elif msg_type == "client/state":
            continue


async def handle_client(ws):
    global stream_served

    raw = await ws.recv()
    hello = json.loads(raw)
    print(f"[server] got hello: {hello.get('type', 'unknown')}", flush=True)

    await ws.send(
        make_text_msg(
            "server/hello",
            {
                "server_id": "pcm32-test-source",
                "name": "PCM32 Test Source",
                "version": 1,
                "active_roles": ["player@v1"],
                "connection_reason": "playback",
            },
        )
    )

    if stream_served:
        print("[server] stream already served once; closing connection", flush=True)
        await ws.close()
        return

    sync_ready = asyncio.Event()
    message_task = asyncio.create_task(handle_client_messages(ws, sync_ready))

    try:
        await wait_for_start_signal()
        try:
            await asyncio.wait_for(sync_ready.wait(), timeout=5.0)
            print("[server] clock sync ready, starting stream", flush=True)
        except asyncio.TimeoutError:
            print("[server] clock sync not ready within 5s, starting anyway", flush=True)
        stream_served = True

        await ws.send(
            make_text_msg(
                "stream/start",
                {
                    "player": {
                        "codec": "pcm",
                        "sample_rate": SAMPLE_RATE,
                        "channels": CHANNELS,
                        "bit_depth": BIT_DEPTH,
                    }
                },
            )
        )
        print("[server] sent stream/start (32-bit PCM)", flush=True)
        await send_audio(ws)
        await ws.send(make_text_msg("stream/end", {}))
        print("[server] sent stream/end", flush=True)
        await ws.close()
    finally:
        message_task.cancel()
        with contextlib.suppress(asyncio.CancelledError):
            await message_task


async def main() -> None:
    generate_reference_capture("/shared/reference_capture.raw")
    print("[server] generated deterministic 32-bit reference capture", flush=True)

    async with websockets.serve(handle_client, "0.0.0.0", PORT):
        print(f"[server] listening on ws://0.0.0.0:{PORT}/sendspin", flush=True)
        await asyncio.sleep(STREAM_DURATION_SECS + 30)


if __name__ == "__main__":
    asyncio.run(main())

#!/bin/sh
set -e

# Generate a 30-second test tone WAV file (1kHz sine, 48kHz, 16-bit, stereo)
python3 -c "
import struct, math, wave

sample_rate = 48000
duration = 30
freq = 1000
amplitude = 16000  # well below 16-bit max

with wave.open('/tmp/test_tone.wav', 'w') as wav:
    wav.setnchannels(2)
    wav.setsampwidth(2)  # 16-bit
    wav.setframerate(sample_rate)
    for i in range(sample_rate * duration):
        val = int(amplitude * math.sin(2 * math.pi * freq * i / sample_rate))
        frame = struct.pack('<hh', val, val)  # stereo
        wav.writeframes(frame)

print(f'Generated: {duration}s, {freq}Hz sine, {sample_rate}Hz, 16-bit stereo WAV')
"

echo "Starting Sendspin server with test tone..."
exec sendspin serve --name "TestSource" --log-level DEBUG /tmp/test_tone.wav

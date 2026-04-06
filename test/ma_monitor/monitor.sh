#!/bin/sh

# Render a compact ASCII waveform of the last N seconds of a raw capture
render_live() {
    file="$1"
    label="$2"
    if [ ! -f "$file" ] || [ "$(stat -c %s "$file" 2>/dev/null || echo 0)" -lt 8 ]; then
        return
    fi
    python3 -c "
import struct, sys

file = '$file'
label = '$label'
sr = 48000
bytes_per_frame = 8
window_sec = 30  # show last 30 seconds
cols = 60
bars = ' ▁▂▃▄▅▆▇█'

with open(file, 'rb') as f:
    f.seek(0, 2)
    total_bytes = f.tell()
    total_frames = total_bytes // bytes_per_frame
    window_frames = min(sr * window_sec, total_frames)
    start_byte = max(0, total_bytes - window_frames * bytes_per_frame)
    f.seek(start_byte)
    data = f.read()

frames = len(data) // bytes_per_frame
if frames == 0:
    sys.exit(0)

block = max(1, frames // cols)
peaks = []
for c in range(min(cols, frames // block)):
    peak = 0
    for i in range(0, block, max(1, block // 16)):
        offset = (c * block + i) * bytes_per_frame
        if offset + 4 <= len(data):
            val = abs(struct.unpack_from('<i', data, offset)[0])
            if val > peak:
                peak = val
    peaks.append(peak)

max_peak = max(peaks) if peaks and max(peaks) > 0 else 1
line = ''
for p in peaks:
    idx = min(int(p / max_peak * (len(bars) - 1)), len(bars) - 1) if max_peak > 0 else 0
    line += bars[idx]

total_sec = total_frames / sr
print(f'  {label} [{total_sec:>6.0f}s] |{line}|')
" 2>/dev/null
}

render_final() {
    file="$1"
    label="$2"
    if [ ! -f "$file" ] || [ "$(stat -c %s "$file" 2>/dev/null || echo 0)" -lt 384000 ]; then
        echo "  $label: no data"
        return
    fi
    python3 -c "
import struct, sys

file = '$file'
label = '$label'
sr = 48000
bytes_per_frame = 8
cols = 72

with open(file, 'rb') as f:
    data = f.read()

total_frames = len(data) // bytes_per_frame
total_sec = total_frames / sr
block_sec = max(1, total_sec / cols)
bars = ' ▁▂▃▄▅▆▇█'

peaks = []
for c in range(min(cols, int(total_sec / block_sec) + 1)):
    start = int(c * block_sec * sr)
    end = min(int((c + 1) * block_sec * sr), total_frames)
    peak = 0
    for f in range(start, end, max(1, (end - start) // 32)):
        offset = f * bytes_per_frame
        if offset + 4 <= len(data):
            val = abs(struct.unpack_from('<i', data, offset)[0])
            if val > peak:
                peak = val
    peaks.append(peak)

if not peaks:
    print(f'  {label}: no data')
    sys.exit(0)

max_peak = max(peaks) if max(peaks) > 0 else 1

print(f'  {label} ({total_sec:.0f}s total, {block_sec:.0f}s/col):')
for row in range(6, 0, -1):
    threshold = max_peak * row / 6
    line = ''
    for p in peaks:
        line += bars[min(int(p / max_peak * 8), 8)] if p >= threshold else ' '
    print(f'  |{line}|')

marks = ''
for c in range(len(peaks)):
    sec = int(c * block_sec)
    if c % 15 == 0:
        m = str(sec) + 's'
        marks += m
    elif len(marks) <= c:
        marks += ' '
print(f'   {marks}')
print()
" 2>/dev/null || echo "  $label: render failed"
}

on_exit() {
    echo ""
    echo "=== Final Waveform Analysis ==="
    echo ""
    render_final /output/bridge1.raw "Bridge1"
    render_final /output/bridge2.raw "Bridge2"
    echo "Raw files: output/bridge1.raw, output/bridge2.raw"
    echo "Convert: sox --no-dither -t raw -e signed-integer -b 32 -c 2 -r 48000 output/bridge1.raw -b 24 output/bridge1.wav"
    exit 0
}

trap on_exit INT TERM

echo "=== spin2dante Interactive MA Test Monitor ==="
echo ""
echo "DANTE devices are visible immediately."
echo "Select Bridge1/Bridge2 as players in Music Assistant."
echo "Press Ctrl-C to stop and see full waveform analysis."
echo ""

sub_rx1=0
sub_rx2=0

while true; do
    devices=$(netaudio device list 2>/dev/null || echo "")

    if [ "$sub_rx1" -eq 0 ]; then
        has_b1=$(echo "$devices" | grep -c "Bridge1" || true)
        has_rx1=$(echo "$devices" | grep -c "rx1" || true)
        if [ "$has_b1" -ge 1 ] && [ "$has_rx1" -ge 1 ]; then
            echo "[$(date +%H:%M:%S)] subscribing rx1 <- Bridge1..."
            netaudio subscription add --tx "01@Bridge1" --rx "01@rx1" 2>&1 \
              && netaudio subscription add --tx "02@Bridge1" --rx "02@rx1" 2>&1 \
              && sub_rx1=1 && echo "  OK" || echo "  FAILED (will retry)"
        fi
    fi

    if [ "$sub_rx2" -eq 0 ]; then
        has_b2=$(echo "$devices" | grep -c "Bridge2" || true)
        has_rx2=$(echo "$devices" | grep -c "rx2" || true)
        if [ "$has_b2" -ge 1 ] && [ "$has_rx2" -ge 1 ]; then
            echo "[$(date +%H:%M:%S)] subscribing rx2 <- Bridge2..."
            netaudio subscription add --tx "01@Bridge2" --rx "01@rx2" 2>&1 \
              && netaudio subscription add --tx "02@Bridge2" --rx "02@rx2" 2>&1 \
              && sub_rx2=1 && echo "  OK" || echo "  FAILED (will retry)"
        fi
    fi

    # Live waveform (last 30s of each capture)
    render_live /output/bridge1.raw "Bridge1"
    render_live /output/bridge2.raw "Bridge2"

    sleep 5
done

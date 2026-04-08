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

# Render waveform from a specific channel in the 4-channel sync capture
render_live_4ch() {
    file="$1"
    label="$2"
    channel="$3"  # 0-based channel index
    if [ ! -f "$file" ] || [ "$(stat -c %s "$file" 2>/dev/null || echo 0)" -lt 16 ]; then
        return
    fi
    python3 -c "
import struct, sys

file = '$file'
label = '$label'
ch = $channel
sr = 48000
bytes_per_frame = 16  # 4 channels * 4 bytes
window_sec = 30
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
        offset = (c * block + i) * bytes_per_frame + ch * 4
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

render_sync() {
    file_a="$1"
    label_a="$2"
    file_b="$3"
    label_b="$4"
    if [ ! -f "$file_a" ] || [ ! -f "$file_b" ]; then
        return
    fi
    python3 -c "
import struct, sys

file_a = '$file_a'
label_a = '$label_a'
file_b = '$file_b'
label_b = '$label_b'
sr = 48000
bytes_per_frame = 8
window_sec = 8
sample_step = 64          # 1.33ms resolution
max_lag_frames = 4800     # +/- 100ms search window
min_peak = 1 << 12

def load_tail(path):
    with open(path, 'rb') as f:
        f.seek(0, 2)
        total_bytes = f.tell()
        total_frames = total_bytes // bytes_per_frame
        window_frames = min(sr * window_sec, total_frames)
        if window_frames <= sample_step * 8:
            return []
        start_byte = total_bytes - window_frames * bytes_per_frame
        f.seek(start_byte)
        data = f.read(window_frames * bytes_per_frame)
    out = []
    for frame in range(0, window_frames, sample_step):
        offset = frame * bytes_per_frame
        if offset + 4 <= len(data):
            out.append(struct.unpack_from('<i', data, offset)[0])
    return out

a = load_tail(file_a)
b = load_tail(file_b)
if len(a) < 32 or len(b) < 32:
    sys.exit(0)

peak = max(max(abs(v) for v in a), max(abs(v) for v in b))
if peak < min_peak:
    print(f'  sync {label_a}<->{label_b}: insufficient signal')
    sys.exit(0)

n = min(len(a), len(b))
a = a[-n:]
b = b[-n:]

mean_a = sum(a) / n
mean_b = sum(b) / n
a = [v - mean_a for v in a]
b = [v - mean_b for v in b]

energy_a = sum(v * v for v in a)
energy_b = sum(v * v for v in b)
if energy_a == 0 or energy_b == 0:
    print(f'  sync {label_a}<->{label_b}: silent window')
    sys.exit(0)

max_lag_steps = min(max_lag_frames // sample_step, n // 3)
best_lag = 0
best_score = None

for lag in range(-max_lag_steps, max_lag_steps + 1):
    if lag >= 0:
        seg_a = a[lag:]
        seg_b = b[:len(seg_a)]
    else:
        seg_b = b[-lag:]
        seg_a = a[:len(seg_b)]
    if len(seg_a) < 16:
        continue
    score = sum(x * y for x, y in zip(seg_a, seg_b))
    if best_score is None or score > best_score:
        best_score = score
        best_lag = lag

lag_frames = best_lag * sample_step
lag_ms = lag_frames * 1000.0 / sr
if lag_frames > 0:
    relation = f'{label_a} leads {label_b}'
elif lag_frames < 0:
    relation = f'{label_b} leads {label_a}'
else:
    relation = 'aligned'
print(f'  sync {label_a}<->{label_b}: {lag_frames:+d} frames ({lag_ms:+.2f}ms), {relation}')
" 2>/dev/null
}

render_sync_precise() {
    file="$1"
    if [ ! -f "$file" ] || [ "$(stat -c %s "$file" 2>/dev/null || echo 0)" -lt 384000 ]; then
        return
    fi
    python3 -c "
import struct, sys, os

file = '$file'
bpf = 16  # 4 channels * 4 bytes (i32)
sr = 48000
window_sec = 5

size = os.path.getsize(file)
total_frames = size // bpf
if total_frames < sr * 2:
    sys.exit(0)

# Read last window_sec seconds
n = min(sr * window_sec, total_frames)
with open(file, 'rb') as f:
    f.seek((total_frames - n) * bpf)
    data = f.read(n * bpf)

frames = len(data) // bpf
if frames < sr:
    sys.exit(0)

# Extract ch01 (Bridge1 L) and ch03 (Bridge2 L)
ch0 = [struct.unpack_from('<i', data, i * bpf)[0] for i in range(frames)]
ch2 = [struct.unpack_from('<i', data, i * bpf + 8)[0] for i in range(frames)]

# Check if both channels have signal
peak0 = max(abs(v) for v in ch0[:1000]) if ch0 else 0
peak2 = max(abs(v) for v in ch2[:1000]) if ch2 else 0
if peak0 < (1 << 16) or peak2 < (1 << 16):
    print(f'  sync: waiting for audio on both bridges')
    sys.exit(0)

# Search for best shift +/- 100ms
test_len = min(2000, frames - 4800)
best_shift = 0; best_matches = 0
for shift in range(-4800, 4801):
    if shift >= 0:
        a = ch0[shift:shift+test_len]
        b = ch2[:test_len]
    else:
        b = ch2[-shift:-shift+test_len]
        a = ch0[:test_len]
    if len(a) < test_len or len(b) < test_len:
        continue
    m = sum(1 for x, y in zip(a, b) if x == y)
    if m > best_matches:
        best_matches = m; best_shift = shift
        if m == test_len: break

if best_matches >= test_len * 0.99:
    # Full verify
    if best_shift >= 0:
        full_m = sum(1 for a, b in zip(ch0[best_shift:], ch2[:frames - abs(best_shift)]) if a == b)
    else:
        full_m = sum(1 for a, b in zip(ch0[:frames - abs(best_shift)], ch2[-best_shift:]) if a == b)
    full_n = frames - abs(best_shift)
    pct = 100 * full_m / full_n if full_n > 0 else 0
    abs_s = abs(best_shift)
    ms = abs_s / 48.0
    if full_m == full_n:
        print(f'  sync: {abs_s} samples ({ms:.2f}ms) offset, bit-perfect ({full_m}/{full_n})')
    else:
        print(f'  sync: {abs_s} samples ({ms:.2f}ms) offset, {pct:.1f}% match ({full_m}/{full_n})')
else:
    if best_matches > 0:
        print(f'  sync: no bit-perfect match (best {best_matches}/{test_len} at {best_shift:+d})')
    else:
        print(f'  sync: no match found')
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

subscribed=0

while true; do
    devices=$(netaudio device list 2>/dev/null || echo "")

    if [ "$subscribed" -eq 0 ]; then
        has_b1=$(echo "$devices" | grep -c "Bridge1" || true)
        has_b2=$(echo "$devices" | grep -c "Bridge2" || true)
        has_rx1=$(echo "$devices" | grep -c "rx1" || true)
        has_rx2=$(echo "$devices" | grep -c "rx2" || true)
        has_sync=$(echo "$devices" | grep -c "rxsync" || true)

        if [ "$has_b1" -ge 1 ] && [ "$has_b2" -ge 1 ] && \
           [ "$has_rx1" -ge 1 ] && [ "$has_rx2" -ge 1 ] && [ "$has_sync" -ge 1 ]; then
            echo "[$(date +%H:%M:%S)] all devices found, creating subscriptions..."
            # Run all subscriptions in parallel
            (netaudio subscription add --tx "01@Bridge1" --rx "01@rx1" 2>/dev/null && \
             netaudio subscription add --tx "02@Bridge1" --rx "02@rx1" 2>/dev/null && \
             echo "  rx1 <- Bridge1: OK") &
            (netaudio subscription add --tx "01@Bridge2" --rx "01@rx2" 2>/dev/null && \
             netaudio subscription add --tx "02@Bridge2" --rx "02@rx2" 2>/dev/null && \
             echo "  rx2 <- Bridge2: OK") &
            (netaudio subscription add --tx "01@Bridge1" --rx "01@rxsync" 2>/dev/null && \
             netaudio subscription add --tx "02@Bridge1" --rx "02@rxsync" 2>/dev/null && \
             netaudio subscription add --tx "01@Bridge2" --rx "03@rxsync" 2>/dev/null && \
             netaudio subscription add --tx "02@Bridge2" --rx "04@rxsync" 2>/dev/null && \
             echo "  rxsync <- Bridge1 + Bridge2: OK") &
            wait
            subscribed=1
            echo "[$(date +%H:%M:%S)] subscriptions complete"
        fi
    fi

    # Live waveform from shared 4-channel capture (same timing reference)
    render_live_4ch /output/sync.raw "Bridge1" 0
    render_live_4ch /output/sync.raw "Bridge2" 2

    # Sync analysis from shared 4-channel capture (bit-perfect, no artifacts)
    render_sync_precise /output/sync.raw

    sleep 5
done

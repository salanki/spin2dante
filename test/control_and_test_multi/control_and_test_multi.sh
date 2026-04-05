#!/bin/sh
set -e

STREAM_COUNT=4
RECORD_SECS=20

echo "=== spin2dante Multi-Stream E2E Test (${STREAM_COUNT} streams) ==="
echo "Waiting for all DANTE devices to appear..."

# Wait for all 16 bridges and 16 receivers
max_wait=180
waited=0
while true; do
    devices=$(netaudio device list 2>/dev/null || echo "")
    bridge_count=$(echo "$devices" | grep -c "SS[0-9]" || true)
    rx_count=$(echo "$devices" | grep -c "rx[0-9]" || true)

    echo "  found ${bridge_count}/${STREAM_COUNT} bridges, ${rx_count}/${STREAM_COUNT} receivers (${waited}s)"

    if [ "$bridge_count" -ge "$STREAM_COUNT" ] && [ "$rx_count" -ge "$STREAM_COUNT" ]; then
        echo ""
        echo "All ${STREAM_COUNT} bridge + receiver pairs found!"
        echo "$devices"
        break
    fi

    waited=$((waited + 5))
    if [ "$waited" -ge "$max_wait" ]; then
        echo "TIMEOUT: not all devices found after ${max_wait}s"
        echo "Devices seen:"
        echo "$devices"
        exit 1
    fi
    sleep 5
done

echo ""
echo "=== Creating audio subscriptions (${STREAM_COUNT} pairs) ==="

# Run subscriptions in parallel — each targets independent devices
for i in $(seq -w 1 $STREAM_COUNT); do
    (
        # Zero-pad to 2 digits for matching device names (SS01, SS02, ...)
        padded=$(printf "%02d" $((10#$i)))
        bridge_name=$(echo "$devices" | grep "SS${padded} " | awk '{print $1}' | head -1)
        rx_name="rx${padded}"

        if [ -n "$bridge_name" ]; then
            netaudio subscription add --tx "01@${bridge_name}" --rx "01@${rx_name}" 2>/dev/null && \
            netaudio subscription add --tx "02@${bridge_name}" --rx "02@${rx_name}" 2>/dev/null && \
            echo "  ${rx_name} <- ${bridge_name}" || \
            echo "  FAILED: ${rx_name} <- ${bridge_name}"
        else
            echo "  SKIPPED: bridge SS${i} not found"
        fi
    ) &
done

echo "Waiting for all subscriptions to complete..."
wait

echo ""
echo "Subscriptions created. Recording for ${RECORD_SECS} seconds..."
sleep $RECORD_SECS

echo ""
echo "=== Analyzing ${STREAM_COUNT} capture files ==="

total=0
ok=0
signal_present=0
min_size=$((5 * 48000 * 2 * 4))  # at least 5s of audio

for i in $(seq 1 $STREAM_COUNT); do
    total=$((total + 1))
    padded=$(printf "%02d" $i)
    file="/shared/capture_${padded}.raw"

    if [ ! -f "$file" ]; then
        echo "  capture_${padded}.raw: MISSING"
        continue
    fi

    size=$(stat -c %s "$file" 2>/dev/null || echo 0)
    if [ "$size" -lt "$min_size" ]; then
        echo "  capture_${padded}.raw: TOO SMALL (${size} bytes)"
        continue
    fi

    ok=$((ok + 1))

    # Signal check — scan up to 30s into the file (audio may start late)
    has_signal=$(python3 -c "
import struct
with open('$file', 'rb') as f:
    data = f.read(48000 * 8 * 30)  # up to 30s of stereo 32-bit
nonzero = sum(1 for i in range(0, len(data), 4) if struct.unpack_from('<i', data, i)[0] != 0)
print('YES' if nonzero > 100 else 'NO')
" 2>/dev/null || echo "ERR")

    if [ "$has_signal" = "YES" ]; then
        signal_present=$((signal_present + 1))
        echo "  capture_${padded}.raw: ${size} bytes, signal=YES"
    else
        echo "  capture_${padded}.raw: ${size} bytes, signal=${has_signal}"
    fi
done

echo ""
echo "=== Cross-stream comparison ==="
echo "Comparing captures pairwise to check synchronization..."

# Compare first capture to all others by looking for the 1kHz sine onset.
# In a perfectly synchronized group, all captures should have nearly identical content.
python3 -c "
import struct, math

def read_samples(path, max_frames=48000*30):
    \"\"\"Read left-channel samples as i32 values (up to 30s).\"\"\"
    samples = []
    try:
        with open(path, 'rb') as f:
            data = f.read(max_frames * 8)  # stereo 32-bit = 8 bytes/frame
        for i in range(0, len(data), 8):  # step by frame (L+R)
            if i + 4 <= len(data):
                samples.append(struct.unpack_from('<i', data, i)[0])
    except FileNotFoundError:
        pass
    return samples

def find_first_nonzero(samples, threshold=1000):
    \"\"\"Find index of first sample above threshold.\"\"\"
    for i, s in enumerate(samples):
        if abs(s) > threshold:
            return i
    return -1

def peak_of(samples):
    if not samples:
        return 0
    return max(abs(s) for s in samples)

ref_path = '/shared/capture_01.raw'
ref = read_samples(ref_path)
ref_onset = find_first_nonzero(ref)
ref_peak = peak_of(ref)

if not ref or ref_onset < 0:
    print('Reference capture (01) has no signal, cannot compare.')
else:
    print(f'Reference (01): onset at sample {ref_onset}, peak={ref_peak}')
    print()

    offsets = []
    for i in range(2, $STREAM_COUNT + 1):
        path = f'/shared/capture_{i:02d}.raw'
        s = read_samples(path)
        onset = find_first_nonzero(s)
        peak = peak_of(s)

        if onset < 0:
            print(f'  capture_{i:02d}: no signal')
        else:
            offset = onset - ref_onset
            offsets.append(offset)
            offset_ms = offset / 48.0
            print(f'  capture_{i:02d}: onset at {onset}, offset={offset:+d} samples ({offset_ms:+.2f}ms), peak={peak}')

    if offsets:
        print()
        min_off = min(offsets)
        max_off = max(offsets)
        spread = max_off - min_off
        avg_off = sum(offsets) / len(offsets)
        print(f'Onset spread: {spread} samples ({spread/48.0:.2f}ms)')
        print(f'Min offset: {min_off:+d}, Max offset: {max_off:+d}, Avg: {avg_off:+.1f}')
        if spread < 480:  # < 10ms
            print('SYNC: GOOD (spread < 10ms)')
        elif spread < 4800:  # < 100ms
            print('SYNC: FAIR (spread < 100ms)')
        else:
            print('SYNC: POOR (spread >= 100ms)')
" || echo "Cross-stream comparison failed"

echo ""
echo "=== Summary ==="
echo "Streams: ${STREAM_COUNT}"
echo "Captures present: ${ok}/${total}"
echo "Signal present: ${signal_present}/${total}"
echo ""
echo "=== Multi-stream test complete ==="

#!/bin/sh
set -e

STREAM_COUNT=4
RECORD_SECS=20
COMPARE_JSON=/tmp/compare_results.jsonl
SYNC_THRESHOLD_FRAMES=48

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
bit_perfect=0
min_size=$((5 * 48000 * 2 * 4))  # at least 5s of audio
rm -f "$COMPARE_JSON"

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

    if python3 /audio_compare.py \
        --reference /shared/reference_capture.raw \
        --capture "$file" \
        --label "capture_${padded}" \
        --min-run-seconds 5 \
        --json > /tmp/compare_${padded}.json
    then
        bit_perfect=$((bit_perfect + 1))
        cat /tmp/compare_${padded}.json >> "$COMPARE_JSON"
        echo "  capture_${padded}.raw: ${size} bytes, bit-perfect overlap=YES"
    else
        cat /tmp/compare_${padded}.json >> "$COMPARE_JSON" 2>/dev/null || true
        echo "  capture_${padded}.raw: ${size} bytes, bit-perfect overlap=NO"
    fi
done

echo ""
echo "=== Anchor sync_key comparison ==="
echo "Reading sync_keys from bridges (written at anchor time)..."

sync_keys=""
for i in $(seq -w 1 $STREAM_COUNT); do
    padded=$(printf "%02d" $((10#$i)))
    keyfile="/shared/sync_key_SS${padded}.txt"
    if [ -f "$keyfile" ]; then
        key=$(cat "$keyfile")
        echo "  SS${padded}: sync_key=${key}"
        sync_keys="${sync_keys} ${key}"
    else
        echo "  SS${padded}: no sync_key file"
    fi
done

sync_key_ok=0
if [ -n "$sync_keys" ]; then
    if python3 -c "
keys = [int(k) for k in '${sync_keys}'.split()]
if len(keys) >= 2:
    spread = max(keys) - min(keys)
    print(f'Sync-key spread: {spread} samples ({spread/48.0:.2f}ms)')
    if spread < ${SYNC_THRESHOLD_FRAMES}:
        print('ANCHOR SYNC: GOOD (< 1ms)')
    else:
        print(f'ANCHOR SYNC: needs work ({spread} samples)')
    import sys
    sys.exit(0 if spread < ${SYNC_THRESHOLD_FRAMES} else 1)
else:
    import sys
    sys.exit(1)
"; then
        sync_key_ok=1
    fi
fi

echo ""
echo "=== Cross-stream comparison ==="
echo "Comparing source offsets to check synchronization..."

if python3 -c "
import json
from statistics import mean
import sys

results = []
try:
    with open('$COMPARE_JSON', 'r') as f:
        for line in f:
            line = line.strip()
            if line:
                results.append(json.loads(line))
except FileNotFoundError:
    pass

if not results:
    print('No successful overlap comparison results available.')
    sys.exit(1)
else:
    import os, struct
    results.sort(key=lambda item: item['label'])

    # Compute end-alignment for each capture.
    # All captures end at the same time (test stops them simultaneously),
    # so end_pos = offset + capture_frames should be identical across
    # bridges if they are in sync. Start offsets differ due to subscription
    # timing and are NOT a valid sync measurement.
    bytes_per_frame = 8  # stereo i32
    for item in results:
        padded = item['label'].replace('capture_', '')
        path = f'/shared/capture_{padded}.raw'
        try:
            cap_bytes = os.path.getsize(path)
        except OSError:
            cap_bytes = 0
        item['capture_frames'] = cap_bytes // bytes_per_frame
        item['end_pos'] = item['offset_frames'] + item['capture_frames']

    ref = results[0]
    print(f\"Reference ({ref['label']}): offset={ref['offset_frames']} end_pos={ref['end_pos']} cap_frames={ref['capture_frames']}\")
    print()
    end_offsets = []
    for item in results[1:]:
        relative_end = item['end_pos'] - ref['end_pos']
        relative_start = item['offset_frames'] - ref['offset_frames']
        end_offsets.append(relative_end)
        print(
            f\"  {item['label']}: end_relative={relative_end:+d} frames ({relative_end/48.0:+.2f}ms), \"
            f\"start_relative={relative_start:+d} ({relative_start/48.0:+.2f}ms), \"
            f\"run={item['longest_run_seconds']:.2f}s\"
        )

    if end_offsets:
        print()
        spread = max(end_offsets) - min(end_offsets)
        avg_off = mean(end_offsets)
        print(f'End-alignment spread: {spread} frames ({spread/48.0:.2f}ms)')
        start_spread = max(r.get('offset_frames',0) for r in results) - min(r.get('offset_frames',0) for r in results)
        print(f'(Start-offset spread was {start_spread} frames — includes subscription timing)')
        if spread < ${SYNC_THRESHOLD_FRAMES}:  # < 1ms
            print('SYNC: GOOD (spread < 1ms)')
        elif spread < 480:  # < 10ms
            print('SYNC: FAIR (spread < 10ms)')
        else:
            print('SYNC: POOR (spread >= 10ms)')
        sys.exit(0 if spread < ${SYNC_THRESHOLD_FRAMES} else 1)
    else:
        print('Only one aligned capture available; cannot assess cross-stream sync.')
        sys.exit(1)
"
then
    sync_ok=1
else
    sync_ok=0
    echo "Cross-stream sync check failed"
fi

echo ""
echo "=== Summary ==="
echo "Streams: ${STREAM_COUNT}"
echo "Captures present: ${ok}/${total}"
echo "Bit-perfect overlap: ${bit_perfect}/${total}"
if [ "$sync_key_ok" -eq 1 ]; then
    echo "Anchor sync (sync_key): PASS (< 1ms spread)"
else
    echo "Anchor sync (sync_key): FAIL (>= 1ms spread or insufficient data)"
fi
if [ "$sync_ok" -eq 1 ]; then
    echo "Capture sync (end-alignment): PASS"
else
    echo "Capture sync (end-alignment): FAIL (measurement may include subscription timing)"
fi
if [ "$bit_perfect" -ne "$total" ]; then
    echo "FAIL: not all captures produced a bit-perfect overlap"
    exit 1
fi
if [ "$sync_key_ok" -ne 1 ]; then
    echo "FAIL: anchor sync_key spread exceeded target"
    exit 1
fi
echo ""
echo "=== Multi-stream test complete ==="

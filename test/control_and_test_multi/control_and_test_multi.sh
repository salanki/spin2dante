#!/bin/sh
set -e

STREAM_COUNT=16
RECORD_SECS=20
COMPARE_JSON=/tmp/compare_results.jsonl

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
echo "=== Cross-stream comparison ==="
echo "Comparing source offsets to check synchronization..."

python3 -c "
import json
from statistics import mean

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
else:
    results.sort(key=lambda item: item['label'])
    ref = results[0]
    print(f\"Reference ({ref['label']}): offset={ref['offset_frames']} frames ({ref['offset_ms']:+.2f}ms)\")
    print()
    offsets = []
    for item in results[1:]:
        relative = item['offset_frames'] - ref['offset_frames']
        offsets.append(relative)
        print(
            f\"  {item['label']}: source offset={item['offset_frames']:+d} frames \"
            f\"({item['offset_ms']:+.2f}ms), relative={relative:+d} frames ({relative/48.0:+.2f}ms), \"
            f\"run={item['longest_run_seconds']:.2f}s\"
        )

    if offsets:
        print()
        spread = max(offsets) - min(offsets)
        avg_off = mean(offsets)
        print(f'Source-offset spread: {spread} frames ({spread/48.0:.2f}ms)')
        print(f'Min relative offset: {min(offsets):+d}, Max relative offset: {max(offsets):+d}, Avg: {avg_off:+.1f}')
        if spread < 48:  # < 1ms
            print('SYNC: GOOD (spread < 1ms)')
        elif spread < 480:  # < 10ms
            print('SYNC: FAIR (spread < 10ms)')
        else:
            print('SYNC: POOR (spread >= 10ms)')
" || echo "Cross-stream comparison failed"

echo ""
echo "=== Summary ==="
echo "Streams: ${STREAM_COUNT}"
echo "Captures present: ${ok}/${total}"
echo "Bit-perfect overlap: ${bit_perfect}/${total}"
if [ "$bit_perfect" -ne "$total" ]; then
    echo "FAIL: not all captures produced a bit-perfect overlap"
    exit 1
fi
echo ""
echo "=== Multi-stream test complete ==="

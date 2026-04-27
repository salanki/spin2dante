#!/bin/sh
set -e

echo "=== Volume Control E2E Test ==="
echo ""

echo "Waiting for devices to appear on the network..."
MAX_WAIT=90
elapsed=0
while [ $elapsed -lt $MAX_WAIT ]; do
    devices=$(netaudio device list 2>/dev/null || echo "")
    bridge_found=$(echo "$devices" | grep -c "SSVolume" || true)
    i2pipe_found=$(echo "$devices" | grep -c "i2pipe" || true)
    if [ "$bridge_found" -ge 1 ] && [ "$i2pipe_found" -ge 1 ]; then
        echo "Both devices found!"
        echo "$devices"
        break
    fi
    echo "  bridge=$bridge_found i2pipe=$i2pipe_found (${elapsed}s)"
    sleep 2
    elapsed=$((elapsed + 2))
done

if [ "$bridge_found" -lt 1 ] || [ "$i2pipe_found" -lt 1 ]; then
    echo "FAIL: devices did not appear within ${MAX_WAIT}s"
    exit 1
fi

echo ""
echo "=== Creating audio subscriptions ==="
bridge_short=$(echo "$devices" | grep "SSVolume" | awk '{print $1}')
bridge_full=$(echo "$devices" | grep "SSVolume" | awk '{print $1, $2}')
echo "Bridge: '$bridge_short'"

if netaudio subscription add --tx "01@${bridge_full}" --rx "01@i2pipe" 2>/dev/null; then
    bridge_name="$bridge_full"
else
    bridge_name="$bridge_short"
    netaudio subscription add --tx "01@${bridge_name}" --rx "01@i2pipe" || echo "Sub 1 failed"
fi
netaudio subscription add --tx "02@${bridge_name}" --rx "02@i2pipe" || echo "Sub 2 failed"
echo "Subscriptions created."

echo ""
echo "Waiting 45s for audio capture (15s full vol + volume change + 25s reduced vol)..."
sleep 45

echo ""
echo "=== Checking bridge logs ==="
if [ -f /shared/bridge.log ]; then
    if grep -q "bridge volume set to" /shared/bridge.log; then
        echo "  PASS: bridge received volume command"
        grep "bridge volume" /shared/bridge.log
    else
        echo "  FAIL: no volume command in bridge logs"
        tail -20 /shared/bridge.log
        exit 1
    fi
else
    echo "  WARNING: bridge log not available"
fi

echo ""
echo "=== Checking capture ==="
if [ ! -f /shared/capture.raw ]; then
    echo "  FAIL: no capture file"
    exit 1
fi
echo "  Capture size: $(wc -c < /shared/capture.raw) bytes"

echo ""
echo "=== Amplitude analysis ==="
python3 /validate.py
result=$?

echo ""
echo "=== Volume control test complete ==="
exit $result

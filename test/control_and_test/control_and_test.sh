#!/bin/sh
set -e

echo "=== spin2dante E2E Test ==="
echo "Waiting for devices to appear on the network..."

# Wait for both devices to be discoverable
max_wait=90
waited=0
while true; do
    devices=$(netaudio device list 2>/dev/null || echo "")
    bridge_found=$(echo "$devices" | grep -c "SSBridge" || true)
    i2pipe_found=$(echo "$devices" | grep -c "i2pipe" || true)

    if [ "$bridge_found" -ge 1 ] && [ "$i2pipe_found" -ge 1 ]; then
        echo "Both devices found!"
        echo "$devices"
        break
    fi

    waited=$((waited + 2))
    if [ "$waited" -ge "$max_wait" ]; then
        echo "TIMEOUT: devices not found after ${max_wait}s"
        echo "Devices seen: $devices"
        exit 1
    fi
    sleep 2
done

echo ""
echo "=== Creating audio subscriptions ==="
# Extract bridge device name dynamically.
# The name may be single-word ("SSBridge") or two-word with hex suffix ("SSBridge ac150004").
# Try the full first two columns; if that fails, try just the first column.
bridge_full=$(echo "$devices" | grep "SSBridge" | awk '{print $1, $2}')
bridge_short=$(echo "$devices" | grep "SSBridge" | awk '{print $1}')
echo "Bridge device name (full): '$bridge_full'"
echo "Bridge device name (short): '$bridge_short'"

# netaudio uses channel@device format — try full name first, fall back to short
if netaudio subscription add --tx "01@${bridge_full}" --rx "01@i2pipe" 2>/dev/null; then
    bridge_name="$bridge_full"
else
    bridge_name="$bridge_short"
    netaudio subscription add --tx "01@${bridge_name}" --rx "01@i2pipe" || echo "Sub 1 failed"
fi
netaudio subscription add --tx "02@${bridge_name}" --rx "02@i2pipe" || echo "Sub 2 failed"
echo "Using bridge name: '$bridge_name'"

touch /shared/start_stream
echo "Subscriptions created. Start signal written. Recording for 20 seconds..."
sleep 20

echo ""
echo "=== Checking capture file ==="
if [ -f /shared/capture.raw ] && [ -f /shared/reference_capture.raw ]; then
    size=$(stat -c %s /shared/capture.raw 2>/dev/null || stat -f %z /shared/capture.raw)
    ref_size=$(stat -c %s /shared/reference_capture.raw 2>/dev/null || stat -f %z /shared/reference_capture.raw)
    echo "Capture file size: $size bytes"
    echo "Reference file size: $ref_size bytes"

    min_size=$((5 * 48000 * 2 * 4))
    if [ "$size" -lt "$min_size" ]; then
        echo "WARNING: capture file smaller than expected ($size < $min_size)"
    else
        echo "Capture file size OK"
    fi

    echo ""
    echo "=== Overlap comparison ==="
    python3 /audio_compare.py \
        --reference /shared/reference_capture.raw \
        --capture /shared/capture.raw \
        --label capture.raw \
        --min-run-seconds 5

else
    echo "NOTE: capture or reference file missing"
    exit 1
fi

echo ""
echo "=== Bridge logs (last 15 lines) ==="
# Can't read other container logs from here, but the output will show in compose logs

echo ""
echo "=== Test complete ==="

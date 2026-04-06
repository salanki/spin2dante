#!/bin/sh
set -e

echo "=== Sendspin Bridge E2E Test ==="
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
# Extract bridge device name dynamically (it includes a random suffix)
bridge_name=$(echo "$devices" | grep "SSBridge" | awk '{print $1, $2}')
echo "Bridge device name: '$bridge_name'"

# netaudio uses channel@device format
netaudio subscription add --tx "01@${bridge_name}" --rx "01@i2pipe" || echo "Sub 1 failed"
netaudio subscription add --tx "02@${bridge_name}" --rx "02@i2pipe" || echo "Sub 2 failed"

echo "Subscriptions created. Recording for 20 seconds..."
sleep 20

echo ""
echo "=== Checking capture file ==="
if [ -f /shared/capture.raw ]; then
    size=$(stat -c %s /shared/capture.raw 2>/dev/null || stat -f %z /shared/capture.raw)
    echo "Capture file size: $size bytes"

    min_size=$((5 * 48000 * 2 * 4))
    if [ "$size" -lt "$min_size" ]; then
        echo "WARNING: capture file smaller than expected ($size < $min_size)"
    else
        echo "Capture file size OK"
    fi

    echo ""
    echo "=== Signal analysis ==="
    python3 -c "
import struct, math

with open('/shared/capture.raw', 'rb') as f:
    data = f.read()

samples = len(data) // 4
nonzero = 0
peak = 0
for i in range(min(samples, 480000)):
    val = struct.unpack_from('<i', data, i * 4)[0]
    if val != 0:
        nonzero += 1
    peak = max(peak, abs(val))

print(f'Total samples: {samples}')
print(f'Non-zero samples (first 10s): {nonzero}/{min(samples, 480000)}')
if peak > 0:
    print(f'Peak value: {peak} ({20 * math.log10(peak / 2147483647):.1f} dBFS)')
else:
    print('Peak: 0 (silence)')
print(f'Signal present: {\"YES\" if nonzero > 1000 else \"NO\"}')
" || echo "Signal analysis failed"

else
    echo "NOTE: no capture file yet (subscriptions may not have been established)"
fi

echo ""
echo "=== Bridge logs (last 15 lines) ==="
# Can't read other container logs from here, but the output will show in compose logs

echo ""
echo "=== Test complete ==="

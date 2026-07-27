#!/bin/sh
set -eu

echo "=== spin2dante deterministic drift correction test ==="
echo "Waiting for the bridge and receiver..."

max_wait=90
waited=0
bridge_found=0
i2pipe_found=0
while [ "$waited" -lt "$max_wait" ]; do
    devices=$(netaudio device list 2>/dev/null || true)
    bridge_found=$(echo "$devices" | grep -c "SSDrift" || true)
    i2pipe_found=$(echo "$devices" | grep -c "i2pipe" || true)
    if [ "$bridge_found" -ge 1 ] && [ "$i2pipe_found" -ge 1 ]; then
        echo "$devices"
        break
    fi
    sleep 2
    waited=$((waited + 2))
done

if [ "$bridge_found" -lt 1 ] || [ "$i2pipe_found" -lt 1 ]; then
    echo "FAIL: devices did not appear within ${max_wait}s"
    exit 1
fi

bridge_full=$(echo "$devices" | grep "SSDrift" | awk '{print $1, $2}')
bridge_short=$(echo "$devices" | grep "SSDrift" | awk '{print $1}')
if netaudio subscription add --tx "01@${bridge_full}" --rx "01@i2pipe" 2>/dev/null; then
    bridge_name="$bridge_full"
else
    bridge_name="$bridge_short"
    netaudio subscription add --tx "01@${bridge_name}" --rx "01@i2pipe"
fi
netaudio subscription add --tx "02@${bridge_name}" --rx "02@i2pipe"
echo "Subscribed i2pipe to ${bridge_name}"

echo "Starting deterministic audio window..."
: > /shared/start_audio
echo "Capturing for 35s (drift checks begin 10s after scheduler anchor)..."
sleep 35

echo "Stopping deterministic audio window..."
: > /shared/stop_audio
waited=0
while ! grep -q "drift correction totals:" /shared/bridge.log 2>/dev/null; do
    if [ "$waited" -ge 10 ]; then
        echo "FAIL: bridge did not log terminal correction totals"
        exit 1
    fi
    sleep 1
    waited=$((waited + 1))
done

# inferno2pipe continues writing until Compose tears the harness down. Analyze a
# stable snapshot so a growing file cannot extend or destabilize the test.
cp /shared/capture.raw /shared/drift_capture.raw

python3 /validate.py

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

python3 /validate.py

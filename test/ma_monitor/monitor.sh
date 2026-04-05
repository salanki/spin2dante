#!/bin/sh
set -e

echo "=== spin2dante Interactive MA Test Monitor ==="
echo ""
echo "Waiting for DANTE devices to appear..."

# Wait for all 4 devices: 2 bridges + 2 receivers
max_wait=120
waited=0
while true; do
    devices=$(netaudio device list 2>/dev/null || echo "")
    bridge1=$(echo "$devices" | grep -c "Bridge1" || true)
    bridge2=$(echo "$devices" | grep -c "Bridge2" || true)
    rx1=$(echo "$devices" | grep -c "rx1" || true)
    rx2=$(echo "$devices" | grep -c "rx2" || true)

    total=$((bridge1 + bridge2 + rx1 + rx2))
    echo "  devices: Bridge1=$bridge1 Bridge2=$bridge2 rx1=$rx1 rx2=$rx2 (${waited}s)"

    if [ "$total" -ge 4 ]; then
        echo ""
        echo "All devices found!"
        echo "$devices"
        break
    fi

    waited=$((waited + 3))
    if [ "$waited" -ge "$max_wait" ]; then
        echo "TIMEOUT: not all devices found after ${max_wait}s"
        echo "Devices seen:"
        echo "$devices"
        echo ""
        echo "Continuing anyway — some subscriptions may fail."
        break
    fi
    sleep 3
done

echo ""
echo "=== Creating DANTE subscriptions ==="

# Extract device names (may have hex suffix)
b1_name=$(echo "$devices" | grep "Bridge1" | awk '{print $1}')
b2_name=$(echo "$devices" | grep "Bridge2" | awk '{print $1}')

# Try short name first, fall back to full
for bridge_var in b1:rx1:"$b1_name" b2:rx2:"$b2_name"; do
    rx=$(echo "$bridge_var" | cut -d: -f2)
    bname=$(echo "$bridge_var" | cut -d: -f3)

    if [ -z "$bname" ]; then
        echo "  SKIP: bridge not found for $rx"
        continue
    fi

    if netaudio subscription add --tx "01@${bname}" --rx "01@${rx}" 2>/dev/null; then
        netaudio subscription add --tx "02@${bname}" --rx "02@${rx}" 2>/dev/null
        echo "  ${rx} <- ${bname} (stereo)"
    else
        echo "  FAILED: ${rx} <- ${bname}"
    fi
done

echo ""
echo "=== Subscriptions complete ==="
echo ""
echo "Now monitoring. Play audio in Music Assistant and select Bridge1/Bridge2 as players."
echo "Press Ctrl-C to stop."
echo ""

# Monitoring loop — runs until ctrl-c
prev_size1=0
prev_size2=0
while true; do
    sleep 10

    size1=$(stat -c %s /output/bridge1.raw 2>/dev/null || echo 0)
    size2=$(stat -c %s /output/bridge2.raw 2>/dev/null || echo 0)

    # Calculate rates
    delta1=$(( (size1 - prev_size1) / 10 ))
    delta2=$(( (size2 - prev_size2) / 10 ))
    prev_size1=$size1
    prev_size2=$size2

    # Duration estimate (48kHz * 2ch * 4bytes = 384000 bytes/sec)
    dur1=$(( size1 / 384000 ))
    dur2=$(( size2 / 384000 ))

    ts=$(date +%H:%M:%S)

    if [ "$delta1" -gt 0 ] || [ "$delta2" -gt 0 ]; then
        echo "[$ts] Bridge1: ${dur1}s captured (${delta1} B/s) | Bridge2: ${dur2}s captured (${delta2} B/s)"
    elif [ "$size1" -gt 0 ] || [ "$size2" -gt 0 ]; then
        echo "[$ts] Bridge1: ${dur1}s (stalled) | Bridge2: ${dur2}s (stalled) — no new audio"
    else
        echo "[$ts] No audio captured yet — is Music Assistant playing to Bridge1/Bridge2?"
    fi
done

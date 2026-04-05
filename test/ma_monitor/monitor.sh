#!/bin/sh

echo "=== spin2dante Interactive MA Test Monitor ==="
echo ""
echo "NOTE: Bridges only appear as DANTE devices after Music Assistant"
echo "starts streaming to them. Select Bridge1/Bridge2 as players in MA."
echo ""
echo "Monitoring for devices and creating subscriptions as they appear..."
echo ""

# Track which subscriptions we've created
sub_rx1=0
sub_rx2=0

while true; do
    devices=$(netaudio device list 2>/dev/null || echo "")

    # Try to subscribe rx1 <- Bridge1 if both exist and not yet subscribed
    if [ "$sub_rx1" -eq 0 ]; then
        has_b1=$(echo "$devices" | grep -c "Bridge1" || true)
        has_rx1=$(echo "$devices" | grep -c "rx1" || true)
        if [ "$has_b1" -ge 1 ] && [ "$has_rx1" -ge 1 ]; then
            echo "[$(date +%H:%M:%S)] Bridge1 and rx1 found, subscribing..."
            netaudio subscription add --tx "01@Bridge1" --rx "01@rx1" 2>&1 \
              && netaudio subscription add --tx "02@Bridge1" --rx "02@rx1" 2>&1 \
              && sub_rx1=1 && echo "  rx1 <- Bridge1 (stereo) OK" \
              || echo "  FAILED (will retry)"
        fi
    fi

    if [ "$sub_rx2" -eq 0 ]; then
        has_b2=$(echo "$devices" | grep -c "Bridge2" || true)
        has_rx2=$(echo "$devices" | grep -c "rx2" || true)
        if [ "$has_b2" -ge 1 ] && [ "$has_rx2" -ge 1 ]; then
            echo "[$(date +%H:%M:%S)] Bridge2 and rx2 found, subscribing..."
            netaudio subscription add --tx "01@Bridge2" --rx "01@rx2" 2>&1 \
              && netaudio subscription add --tx "02@Bridge2" --rx "02@rx2" 2>&1 \
              && sub_rx2=1 && echo "  rx2 <- Bridge2 (stereo) OK" \
              || echo "  FAILED (will retry)"
        fi
    fi

    # Report capture status
    size1=$(stat -c %s /output/bridge1.raw 2>/dev/null || echo 0)
    size2=$(stat -c %s /output/bridge2.raw 2>/dev/null || echo 0)
    dur1=$(( size1 / 384000 ))
    dur2=$(( size2 / 384000 ))

    ts=$(date +%H:%M:%S)

    status1="waiting"
    status2="waiting"
    [ "$sub_rx1" -eq 1 ] && status1="subscribed"
    [ "$size1" -gt 0 ] && status1="${dur1}s captured"
    [ "$sub_rx2" -eq 1 ] && status2="subscribed"
    [ "$size2" -gt 0 ] && status2="${dur2}s captured"

    echo "[$ts] Bridge1: $status1 | Bridge2: $status2"

    sleep 10
done

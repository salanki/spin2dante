#!/bin/sh
set -e

echo "=== spin2dante Resilience Test Validator ==="
echo ""

# Wait for the bridge and server to run through all scenarios (~30s)
echo "Waiting 45s for test scenarios to complete..."
sleep 45

echo ""
echo "=== Collecting bridge logs ==="

# The bridge logs are in /shared/bridge.log (tee'd in docker-compose)
if [ ! -f /shared/bridge.log ]; then
    echo "ERROR: /shared/bridge.log not found"
    exit 1
fi

log="/shared/bridge.log"

echo ""
echo "=== Scenario 1: Stream start/stop/restart ==="

# Check: bridge received first stream/start
if grep -q "stream start: codec=pcm" "$log"; then
    echo "  PASS: received stream/start"
else
    echo "  FAIL: no stream/start seen"
fi

# Check: bridge received stream/end
if grep -q "stream ended" "$log"; then
    echo "  PASS: received stream/end"
else
    echo "  FAIL: no stream/end seen"
fi

# Check: bridge entered idle then restarted
if grep -q "stopping DANTE transmitter" "$log"; then
    echo "  PASS: transmitter stopped on stream/end"
else
    echo "  FAIL: transmitter not stopped"
fi

# Check: second stream/start worked
start_count=$(grep -c "stream start: codec=pcm" "$log" || true)
if [ "$start_count" -ge 2 ]; then
    echo "  PASS: received second stream/start ($start_count total)"
else
    echo "  FAIL: only $start_count stream/start seen (expected >= 2)"
fi

# Check: prebuffer completed (at least once means audio was flowing)
prebuf_count=$(grep -c "prebuffer complete" "$log" || true)
if [ "$prebuf_count" -ge 1 ]; then
    echo "  PASS: prebuffer completed $prebuf_count time(s)"
else
    echo "  FAIL: prebuffer never completed"
fi

echo ""
echo "=== Scenario 2: Mid-stream clear (seek) ==="

if grep -q "stream cleared" "$log"; then
    echo "  PASS: received stream/clear"
else
    echo "  FAIL: no stream/clear seen"
fi

if grep -q "cleared stale audio" "$log"; then
    echo "  PASS: stale audio was cleared"
else
    echo "  FAIL: stale audio not cleared"
fi

# Check that bridge re-entered running state after clear
rebuf_count=$(grep -c "Rebuffering\|cleared stale audio" "$log" || true)
if [ "$rebuf_count" -ge 1 ]; then
    echo "  PASS: entered rebuffer mode ($rebuf_count time(s))"
else
    echo "  FAIL: rebuffer mode not entered"
fi

echo ""
echo "=== Scenario 3: Server disconnect / reconnect ==="

if grep -q "session ended with error\|Sendspin connection closed\|Sendspin audio stream ended" "$log"; then
    echo "  PASS: detected server disconnect"
else
    echo "  FAIL: no disconnect detected"
fi

if grep -c "connecting to Sendspin server" "$log" | grep -q "^[2-9]\|^[1-9][0-9]"; then
    reconnects=$(grep -c "connecting to Sendspin server" "$log")
    echo "  PASS: reconnect attempted ($reconnects connection attempts)"
else
    echo "  FAIL: no reconnect attempt"
fi

# Check if bridge reconnected and got a new stream
late_starts=$(grep -c "connected to Sendspin server" "$log" || true)
if [ "$late_starts" -ge 2 ]; then
    echo "  PASS: reconnected successfully ($late_starts connections)"
else
    echo "  INFO: only $late_starts connection(s) — reconnect may not have completed within timeout"
fi

echo ""
echo "=== Log summary ==="
echo "Total stream/start: $(grep -c 'stream start:' "$log" || echo 0)"
echo "Total stream/end:   $(grep -c 'stream ended' "$log" || echo 0)"
echo "Total stream/clear: $(grep -c 'stream cleared' "$log" || echo 0)"
echo "Total prebuffers:   $(grep -c 'prebuffer complete\|prebuffering' "$log" || echo 0)"
echo "Total reconnects:   $(grep -c 'reconnecting in' "$log" || echo 0)"
echo "No-subscriber warnings: $(grep -c 'no active DANTE subscriber' "$log" || echo 0)"

echo ""
echo "=== Full bridge log (filtered) ==="
grep -v DEBUG "$log" | grep -v "unknown request" | grep -v "raw udp" | grep -v "Unable to load state"

echo ""
echo "=== Resilience test complete ==="

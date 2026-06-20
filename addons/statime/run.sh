#!/usr/bin/with-contenv bashio
set -euo pipefail

PTP_INTERFACE="$(bashio::config 'ptp_interface')"
CLOCK_PATH="$(bashio::config 'clock_path')"
LOG_LEVEL="$(bashio::config 'log_level')"

if [[ "$PTP_INTERFACE" == "auto" ]]; then
    PTP_INTERFACE="$(ip route show default | awk '{print $5}' | head -1)"
fi

if [[ -z "$PTP_INTERFACE" ]]; then
    bashio::log.fatal "Could not determine the network interface. Set 'ptp_interface' explicitly."
    exit 1
fi

cp /etc/statime.toml.template /etc/statime.toml
sed -i "s|loglevel = \"info\"|loglevel = \"${LOG_LEVEL}\"|" /etc/statime.toml
sed -i "s|usrvclock-path = \"/shared/usrvclock\"|usrvclock-path = \"${CLOCK_PATH}\"|" /etc/statime.toml
sed -i "s|interface = \"eth0\"|interface = \"${PTP_INTERFACE}\"|" /etc/statime.toml
mkdir -p "$(dirname "$CLOCK_PATH")"

bashio::log.info "Starting statime"
bashio::log.info "Interface: $PTP_INTERFACE"
bashio::log.info "Clock path: $CLOCK_PATH"

# Run the PTP daemon at a real-time (SCHED_FIFO) priority so it isn't starved by
# the normal-priority load on the audio-bridge node (the clock is the most
# contention-sensitive piece — a starved daemon lets the media clock jump, which
# shows up as inferno flows_tx "media clock jumped, dropout occurs" across all
# zones at once). Priority 80 sits just below the audio TX threads (FF 81) on
# purpose, so this can't preempt audio. Requires `realtime: true` in config.yaml
# (CAP_SYS_NICE + rtprio); fall back to normal scheduling if chrt is unavailable
# or lacks permission.
if command -v chrt >/dev/null 2>&1 && chrt -f 80 true 2>/dev/null; then
    bashio::log.info "Scheduling statime at SCHED_FIFO priority 80"
    exec chrt -f 80 /usr/local/bin/statime -c /etc/statime.toml
else
    bashio::log.warning "chrt unavailable or no rtprio permission; starting statime at normal priority"
    exec /usr/local/bin/statime -c /etc/statime.toml
fi

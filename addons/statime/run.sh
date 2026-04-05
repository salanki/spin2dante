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

exec /usr/local/bin/statime -c /etc/statime.toml

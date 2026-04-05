#!/usr/bin/with-contenv bashio
set -euo pipefail

URL="$(bashio::config 'url')"
NAME="$(bashio::config 'name')"
BUFFER_MS="$(bashio::config 'buffer_ms')"
CLOCK_PATH="$(bashio::config 'clock_path')"
LOG_LEVEL="$(bashio::config 'log_level')"

if [[ -z "$URL" ]]; then
    bashio::log.fatal "The 'url' option is required"
    exit 1
fi

export RUST_LOG="$LOG_LEVEL"
export INFERNO_CLOCK_PATH="$CLOCK_PATH"
export INFERNO_TX_CHANNELS="2"
export INFERNO_RX_CHANNELS="0"
export INFERNO_SAMPLE_RATE="48000"
export TMPDIR="/share/tmp_spin2dante"
mkdir -p "$TMPDIR"

bashio::log.info "Starting spin2dante"
bashio::log.info "URL: $URL"
bashio::log.info "Device name: $NAME"
bashio::log.info "Clock path: $CLOCK_PATH"

exec /usr/local/bin/spin2dante \
    --url "$URL" \
    --name "$NAME" \
    --buffer-ms "$BUFFER_MS"

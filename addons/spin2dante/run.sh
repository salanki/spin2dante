#!/usr/bin/with-contenv bashio
set -euo pipefail

CLOCK_PATH="$(bashio::config 'clock_path')"
WAIT_FOR_CLOCK_SECONDS="$(bashio::config 'wait_for_clock_seconds')"
LOG_LEVEL="$(bashio::config 'log_level')"
OPTIONS_FILE=/data/options.json

if [[ ! -f "$OPTIONS_FILE" ]]; then
    bashio::log.fatal "Missing options file: $OPTIONS_FILE"
    exit 1
fi

BRIDGE_COUNT="$(jq '.bridges | length' "$OPTIONS_FILE")"
if [[ "$BRIDGE_COUNT" -eq 0 ]]; then
    bashio::log.fatal "Configure at least one bridge entry before starting the add-on"
    exit 1
fi

export RUST_LOG="$LOG_LEVEL"
export INFERNO_CLOCK_PATH="$CLOCK_PATH"
export INFERNO_TX_CHANNELS="2"
export INFERNO_RX_CHANNELS="0"
export INFERNO_SAMPLE_RATE="48000"

declare -A IDS=()
declare -A PROCESS_IDS=()
declare -A ALT_PORTS=()
declare -a ALT_PORT_VALUES=()
declare -a PIDS=()

for ((i = 0; i < BRIDGE_COUNT; i++)); do
    id="$(jq -r ".bridges[$i].id" "$OPTIONS_FILE")"
    name="$(jq -r ".bridges[$i].name" "$OPTIONS_FILE")"
    process_id="$(jq -r ".bridges[$i].process_id" "$OPTIONS_FILE")"
    alt_port="$(jq -r ".bridges[$i].alt_port" "$OPTIONS_FILE")"

    if [[ -n "${IDS[$id]:-}" ]]; then
        bashio::log.fatal "Duplicate bridge id: $id"
        exit 1
    fi
    IDS[$id]="$name"

    if [[ -n "${PROCESS_IDS[$process_id]:-}" ]]; then
        bashio::log.fatal "Duplicate process_id: $process_id"
        exit 1
    fi
    PROCESS_IDS[$process_id]="$id"

    if [[ -n "${ALT_PORTS[$alt_port]:-}" ]]; then
        bashio::log.fatal "Duplicate alt_port: $alt_port"
        exit 1
    fi
    ALT_PORTS[$alt_port]="$id"

    for used_alt_port in "${ALT_PORT_VALUES[@]:-}"; do
        diff=$((alt_port - used_alt_port))
        if (( diff < 0 )); then
            diff=$(( -diff ))
        fi
        if (( diff < 10 )); then
            bashio::log.fatal "alt_port values must be spaced by at least 10: $alt_port conflicts with $used_alt_port"
            exit 1
        fi
    done
    ALT_PORT_VALUES+=("$alt_port")

done

if (( WAIT_FOR_CLOCK_SECONDS > 0 )); then
    bashio::log.info "Waiting for clock socket at $CLOCK_PATH"
    for ((elapsed = 0; elapsed < WAIT_FOR_CLOCK_SECONDS; elapsed++)); do
        if [[ -S "$CLOCK_PATH" ]]; then
            break
        fi
        sleep 1
    done
    if [[ ! -S "$CLOCK_PATH" ]]; then
        bashio::log.fatal "Clock socket did not appear within ${WAIT_FOR_CLOCK_SECONDS}s: $CLOCK_PATH"
        exit 1
    fi
fi

terminate_children() {
    local pid
    for pid in "${PIDS[@]:-}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
}

trap 'bashio::log.info "Stopping bridge processes"; terminate_children; exit 0' SIGTERM SIGINT

for ((i = 0; i < BRIDGE_COUNT; i++)); do
    id="$(jq -r ".bridges[$i].id" "$OPTIONS_FILE")"
    name="$(jq -r ".bridges[$i].name" "$OPTIONS_FILE")"
    url="$(jq -r ".bridges[$i].url" "$OPTIONS_FILE")"
    buffer_ms="$(jq -r ".bridges[$i].buffer_ms" "$OPTIONS_FILE")"
    process_id="$(jq -r ".bridges[$i].process_id" "$OPTIONS_FILE")"
    alt_port="$(jq -r ".bridges[$i].alt_port" "$OPTIONS_FILE")"
    tmpdir="/share/tmp_${id}"

    mkdir -p "$tmpdir"

    bashio::log.info "Starting bridge '$id' (${name}) on alt_port=${alt_port}, process_id=${process_id}"
    TMPDIR="$tmpdir" \
    INFERNO_PROCESS_ID="$process_id" \
    INFERNO_ALT_PORT="$alt_port" \
    /usr/local/bin/spin2dante \
        --url "$url" \
        --name "$name" \
        --buffer-ms "$buffer_ms" &

    PIDS+=("$!")
done

set +e
wait -n "${PIDS[@]}"
status=$?
set -e

bashio::log.error "A bridge process exited with status $status; stopping remaining bridges"
terminate_children
exit "$status"

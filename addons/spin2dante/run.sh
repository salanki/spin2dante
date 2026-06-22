#!/usr/bin/with-contenv bashio
set -euo pipefail

CLOCK_PATH="$(bashio::config 'clock_path')"
WAIT_FOR_CLOCK_SECONDS="$(bashio::config 'wait_for_clock_seconds')"
LOG_LEVEL="$(bashio::config 'log_level')"
DANTE_BIND="$(bashio::config 'dante_bind')"
SERVER_BUFFER_MS="$(bashio::config 'server_buffer_ms')"
DRIFT_THRESHOLD_MS="$(bashio::config 'drift_threshold_ms')"
DRIFT_CHECK_INTERVAL_MS="$(bashio::config 'drift_check_interval_ms')"
MAX_CORRECTION_SAMPLES_PER_TICK="$(bashio::config 'max_correction_samples_per_tick')"
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

if [[ "$DANTE_BIND" == "auto" ]]; then
    DANTE_BIND=""
fi

declare -A IDS=()
declare -A PROCESS_IDS=()
declare -A ALT_PORTS=()
declare -A BUFFER_VALUES=()
declare -A LATENCY_VALUES=()
declare -a ALT_PORT_VALUES=()
declare -a PIDS=()

for ((i = 0; i < BRIDGE_COUNT; i++)); do
    id="$(jq -r ".bridges[$i].id" "$OPTIONS_FILE")"
    name="$(jq -r ".bridges[$i].name" "$OPTIONS_FILE")"
    process_id="$(jq -r ".bridges[$i].process_id" "$OPTIONS_FILE")"
    alt_port="$(jq -r ".bridges[$i].alt_port" "$OPTIONS_FILE")"
    buffer_ms="$(jq -r ".bridges[$i].buffer_ms" "$OPTIONS_FILE")"

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
    dante_latency="$(jq -r ".bridges[$i].dante_latency // \"10\"" "$OPTIONS_FILE")"
    BUFFER_VALUES[$buffer_ms]=1
    LATENCY_VALUES[$dante_latency]=1

done

if (( ${#BUFFER_VALUES[@]} > 1 )); then
    bashio::log.warning "Mixed buffer_ms values detected across bridges. buffer_ms is real playout latency, not just jitter tolerance, so bridges with different values will not stay sample-aligned."
fi

if (( ${#LATENCY_VALUES[@]} > 1 )); then
    bashio::log.warning "Mixed dante_latency values detected across bridges. Receivers subscribing to different bridges will use different playout buffers."
fi

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

if [[ -n "$DANTE_BIND" ]]; then
    bashio::log.info "DANTE bind: $DANTE_BIND"
else
    bashio::log.info "DANTE bind: auto"
fi

for ((i = 0; i < BRIDGE_COUNT; i++)); do
    id="$(jq -r ".bridges[$i].id" "$OPTIONS_FILE")"
    name="$(jq -r ".bridges[$i].name" "$OPTIONS_FILE")"
    url="$(jq -r ".bridges[$i].url" "$OPTIONS_FILE")"
    buffer_ms="$(jq -r ".bridges[$i].buffer_ms" "$OPTIONS_FILE")"
    process_id="$(jq -r ".bridges[$i].process_id" "$OPTIONS_FILE")"
    alt_port="$(jq -r ".bridges[$i].alt_port" "$OPTIONS_FILE")"
    tmpdir="/share/tmp_${id}"

    mkdir -p "$tmpdir"

    dante_latency="$(jq -r ".bridges[$i].dante_latency // \"10\"" "$OPTIONS_FILE")"
    # Per-bridge server_buffer_ms overrides the global default when present.
    server_buffer_ms="$(jq -r ".bridges[$i].server_buffer_ms // \"$SERVER_BUFFER_MS\"" "$OPTIONS_FILE")"
    volume_control="$(jq -r ".bridges[$i].volume_control // \"none\"" "$OPTIONS_FILE")"
    report_dante_subscriber="$(jq -r ".bridges[$i].report_dante_subscriber // false" "$OPTIONS_FILE")"

    extra_args=()
    if [[ "$report_dante_subscriber" == "true" ]]; then
        extra_args+=(--report-dante-subscriber)
    fi
    if [[ "$volume_control" == "bridge" ]]; then
        extra_args+=(--state-file "/data/volume_state_${id}.json")
    fi

    bridge_env=(
        HOME="/data"
        TMPDIR="$tmpdir"
        INFERNO_PROCESS_ID="$process_id"
        INFERNO_ALT_PORT="$alt_port"
    )
    if [[ -n "$DANTE_BIND" ]]; then
        bridge_env+=(INFERNO_BIND_IP="$DANTE_BIND")
    fi

    bashio::log.info "Starting bridge '$id' (${name}) on alt_port=${alt_port}, process_id=${process_id}, dante_latency=${dante_latency}, server_buffer_ms=${server_buffer_ms}, volume_control=${volume_control}"
    env "${bridge_env[@]}" \
    /usr/local/bin/spin2dante \
        --url "$url" \
        --name "$name" \
        --buffer-ms "$buffer_ms" \
        --server-buffer-ms "$server_buffer_ms" \
        --dante-latency "$dante_latency" \
        --drift-threshold-ms "$DRIFT_THRESHOLD_MS" \
        --drift-check-interval-ms "$DRIFT_CHECK_INTERVAL_MS" \
        --max-correction-samples-per-tick "$MAX_CORRECTION_SAMPLES_PER_TICK" \
        --volume-control "$volume_control" \
        "${extra_args[@]}" &

    PIDS+=("$!")
done

set +e
wait -n "${PIDS[@]}"
status=$?
set -e

bashio::log.error "A bridge process exited with status $status; stopping remaining bridges"
terminate_children
exit "$status"

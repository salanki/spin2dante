#!/usr/bin/env sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
DOCKER_CONFIG_DIR=${DOCKER_CONFIG_DIR:-/tmp/music-assistant-docker-config}

exec docker --config "$DOCKER_CONFIG_DIR" compose -f "$SCRIPT_DIR/docker-compose.yml" down

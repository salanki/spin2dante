#!/usr/bin/env sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
DOCKER_CONFIG_DIR=${DOCKER_CONFIG_DIR:-/tmp/music-assistant-docker-config}

mkdir -p "$SCRIPT_DIR/data" "$SCRIPT_DIR/media"
mkdir -p "$DOCKER_CONFIG_DIR"

if [ ! -f "$DOCKER_CONFIG_DIR/config.json" ]; then
  printf '{"auths":{}}\n' > "$DOCKER_CONFIG_DIR/config.json"
fi

exec docker --config "$DOCKER_CONFIG_DIR" compose -f "$SCRIPT_DIR/docker-compose.yml" up -d

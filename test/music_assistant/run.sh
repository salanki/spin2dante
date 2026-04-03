#!/usr/bin/env sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

mkdir -p "$SCRIPT_DIR/data" "$SCRIPT_DIR/media"

exec docker compose -f "$SCRIPT_DIR/docker-compose.yml" up -d


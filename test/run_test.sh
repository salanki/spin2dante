#!/bin/sh
set -e

echo "=== Building and running spin2dante E2E test ==="
echo ""

cd "$(dirname "$0")"

# Build inferno2pipe image (needed for DANTE receive)
echo "Building inferno2pipe image..."
docker build -f ../../../inferno/Dockerfile.alpine-i2pipe -t inferno_aoip:alpine-i2pipe ../../../inferno/

echo ""
echo "Starting test environment..."
docker compose down --remove-orphans 2>/dev/null || true
docker compose up --build --abort-on-container-exit control_and_test

echo ""
echo "Cleaning up..."
docker compose down --remove-orphans

#!/bin/sh
set -e

python3 /generate_reference_signal.py \
  --wav-path /tmp/test_signal.wav \
  --capture-raw-path /shared/reference_capture.raw

echo "Starting Sendspin server with deterministic reference signal..."
exec sendspin serve --name "TestSource" --log-level DEBUG /tmp/test_signal.wav

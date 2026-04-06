#!/bin/sh
set -e

SRC_CONFIG="${STATIME_CONFIG:-/etc/statime.toml}"
CONFIG="/tmp/statime-run.toml"

# Copy config to writable location (bind mounts may be read-only)
cp "$SRC_CONFIG" "$CONFIG"

# Auto-detect the primary network interface if not specified
if [ -z "$PTP_INTERFACE" ]; then
    PTP_INTERFACE=$(ip route show default | awk '{print $5}' | head -1)
    echo "Auto-detected interface: $PTP_INTERFACE"
fi

if [ -z "$PTP_INTERFACE" ]; then
    echo "ERROR: Could not detect network interface. Set PTP_INTERFACE env var."
    exit 1
fi

# Patch the config with the detected interface
sed -i "s/interface = \"eth0\"/interface = \"$PTP_INTERFACE\"/" "$CONFIG"

echo "Starting Statime PTP daemon on interface $PTP_INTERFACE (config: $SRC_CONFIG)..."
exec statime -c "$CONFIG"

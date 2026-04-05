#!/bin/sh
set -e

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
sed -i "s/interface = \"eth0\"/interface = \"$PTP_INTERFACE\"/" /etc/statime.toml

echo "Starting Statime PTP daemon on interface $PTP_INTERFACE..."
exec statime -c /etc/statime.toml

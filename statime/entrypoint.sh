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

if ! ip link show "$PTP_INTERFACE" >/dev/null 2>&1; then
    echo "ERROR: interface '$PTP_INTERFACE' does not exist in this container."
    echo "       Available: $(ip -o link show | awk -F': ' '{print $2}' | sed 's/@.*//' | tr '\n' ' ')"
    echo "       Set PTP_INTERFACE to your DANTE-facing NIC, and make sure the"
    echo "       container uses host networking."
    exit 1
fi

# PTP needs to run on the same NIC the bridges advertise themselves on. A NIC
# with no address, or only a link-local one, means we are almost certainly on
# the wrong interface — warn rather than fail, since link-local DANTE networks
# are legal.
IFACE_IPV4=$(ip -4 -o addr show dev "$PTP_INTERFACE" | awk '{print $4}' | tr '\n' ' ')
if [ -z "$IFACE_IPV4" ]; then
    echo "WARNING: interface '$PTP_INTERFACE' has no IPv4 address; PTP will likely not sync."
else
    echo "Interface $PTP_INTERFACE addresses: $IFACE_IPV4"
fi

# Patch the config with the detected interface
sed -i "s/interface = \"eth0\"/interface = \"$PTP_INTERFACE\"/" "$CONFIG"

echo "Starting Statime PTP daemon on interface $PTP_INTERFACE (config: $SRC_CONFIG)..."
exec statime -c "$CONFIG"

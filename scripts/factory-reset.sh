#!/usr/bin/env bash
# ADOS Drone Agent — Factory Reset
# Wipes config, device identity, and certs. Agent will re-enter setup mode on next boot.
set -euo pipefail

CONFIG_DIR="/etc/ados"

echo "=== ADOS Drone Agent — Factory Reset ==="
echo "This will remove:"
echo "  - Device identity ($CONFIG_DIR/device-id)"
echo "  - Configuration ($CONFIG_DIR/config.yaml)"
echo "  - TLS certificates ($CONFIG_DIR/certs/)"
echo "  - Pairing state ($CONFIG_DIR/pairing.json)"
echo "  - Log files (/var/log/ados/)"
echo ""

if [ "${1:-}" != "--force" ]; then
    read -p "Continue? [y/N] " confirm
    if [ "$confirm" != "y" ] && [ "$confirm" != "Y" ]; then
        echo "Aborted."
        exit 0
    fi
fi

# Stop service(s)
systemctl stop ados-supervisor 2>/dev/null || true
systemctl stop ados-agent 2>/dev/null || true
systemctl stop ados.service 2>/dev/null || true

# Wipe
rm -f "$CONFIG_DIR/device-id"
rm -f "$CONFIG_DIR/config.yaml"
rm -f "$CONFIG_DIR/pairing.json"
rm -rf "$CONFIG_DIR/certs/"
rm -rf /var/log/ados/*

echo "Factory reset complete. Reboot to enter setup mode."
echo "  sudo reboot"

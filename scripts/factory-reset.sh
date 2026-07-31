#!/usr/bin/env bash
# ADOS Drone Agent — Factory Reset
# Wipes config, device identity, and certs. Agent will re-enter setup mode on next boot.
set -euo pipefail

CONFIG_DIR="/etc/ados"

echo "=== ADOS Drone Agent — Factory Reset ==="
echo "This will remove every standing credential:"
echo "  - Pairing state / API key   ($CONFIG_DIR/pairing.json)"
echo "  - Dashboard PIN             ($CONFIG_DIR/dashboard-pin.json)"
echo "  - MCP token                 ($CONFIG_DIR/mcp-token.json)"
echo "  - Setup / tunnel secrets    ($CONFIG_DIR/secrets/)"
echo "  - Access point passphrase   ($CONFIG_DIR/ap-passphrase)"
echo "  - Radio keypair             ($CONFIG_DIR/wfb/)"
echo "  - TLS certificates          ($CONFIG_DIR/certs/)"
echo "  - Configuration             ($CONFIG_DIR/config.yaml)"
echo "  - Device identity           ($CONFIG_DIR/device-id)"
echo "  - Log files                 (/var/log/ados/)"
echo ""
echo "Kept: $CONFIG_DIR/profile.conf — which profile and channel this box"
echo "runs. It holds no secret, and without it a later bare upgrade can"
echo "reprofile the box."
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

# Wipe. This list is mirrored by FACTORY_RESET_FILES / FACTORY_RESET_DIRS in
# `src/ados/core/paths.py`, and a test asserts the two agree — the reset paths
# diverged in the first place because each carried its own copy.
#
# Credentials first, so an interrupted run has already destroyed the things
# that grant access rather than only the things that identify the box.
rm -f "$CONFIG_DIR/pairing.json"
rm -f "$CONFIG_DIR/dashboard-pin.json"
rm -f "$CONFIG_DIR/mcp-token.json"
rm -f "$CONFIG_DIR/ap-passphrase"
rm -rf "$CONFIG_DIR/secrets/"
rm -rf "$CONFIG_DIR/wfb/"
rm -rf "$CONFIG_DIR/certs/"
rm -f /var/lib/ados/setup-complete

# Configuration and identity.
rm -f "$CONFIG_DIR/device-id"
rm -f "$CONFIG_DIR/config.yaml"
rm -rf /var/log/ados/*

echo "Factory reset complete. Reboot to enter setup mode."
echo "  sudo reboot"

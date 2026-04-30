#!/usr/bin/env bash
# Install the systemd Slice unit that hosts the per-plugin services.
#
# Called by install.sh during agent provisioning. Idempotent: re-running
# is safe and only triggers a daemon-reload if the slice content actually
# changed. Should run as root (writes to /etc/systemd/system/).

set -euo pipefail

SLICE_PATH="/etc/systemd/system/ados-plugins.slice"
LOG_DIR="/var/log/ados/plugins"
DATA_DIR="/var/ados/plugin-data"
INSTALL_DIR="/var/ados/plugins"
RUN_DIR="/run/ados/plugins"
KEYS_DIR="/etc/ados/plugin-keys"

NEW_CONTENT=$(cat <<'EOF'
[Unit]
Description=ADOS plugin shared cgroup slice
Before=slices.target

[Slice]
CPUAccounting=yes
MemoryAccounting=yes
TasksAccounting=yes
IOAccounting=yes
EOF
)

mkdir -p "$LOG_DIR" "$DATA_DIR" "$INSTALL_DIR" "$KEYS_DIR"
chmod 0755 "$LOG_DIR" "$DATA_DIR" "$INSTALL_DIR"
chmod 0700 "$KEYS_DIR"

# RUN_DIR is on tmpfs and recreated on boot; install.sh does this
# alongside the other /run/ados subdirs.
mkdir -p "$RUN_DIR" 2>/dev/null || true

if [[ -f "$SLICE_PATH" ]] && diff -q <(echo "$NEW_CONTENT") "$SLICE_PATH" >/dev/null 2>&1; then
    echo "ados-plugins.slice already installed and unchanged"
    exit 0
fi

printf '%s\n' "$NEW_CONTENT" > "$SLICE_PATH"
chmod 0644 "$SLICE_PATH"
systemctl daemon-reload
echo "ados-plugins.slice installed at $SLICE_PATH"

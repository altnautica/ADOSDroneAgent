#!/usr/bin/env bash
# ADOS Drone Agent — Installation Script
# Supports: Raspberry Pi OS (Bookworm), Ubuntu 22.04+
set -euo pipefail

INSTALL_DIR="/opt/ados"
CONFIG_DIR="/etc/ados"
DATA_DIR="/var/ados"
VENV_DIR="${INSTALL_DIR}/venv"
SERVICE_FILE="/etc/systemd/system/ados-agent.service"

echo "=== ADOS Drone Agent Installer ==="
echo ""

# Check root
if [ "$(id -u)" -ne 0 ]; then
    echo "Error: Run as root (sudo ./install.sh)"
    exit 1
fi

# Check OS
if [ -f /etc/os-release ]; then
    . /etc/os-release
    echo "OS: ${PRETTY_NAME:-unknown}"
else
    echo "Warning: Cannot detect OS"
fi

# Check Python 3.11+
PYTHON=""
for py in python3.12 python3.11 python3; do
    if command -v "$py" &>/dev/null; then
        ver=$("$py" -c 'import sys; print(f"{sys.version_info.major}.{sys.version_info.minor}")')
        major=$(echo "$ver" | cut -d. -f1)
        minor=$(echo "$ver" | cut -d. -f2)
        if [ "$major" -ge 3 ] && [ "$minor" -ge 11 ]; then
            PYTHON="$py"
            echo "Python: $PYTHON ($ver)"
            break
        fi
    fi
done

if [ -z "$PYTHON" ]; then
    echo "Error: Python 3.11+ required. Install with:"
    echo "  sudo apt install python3.11 python3.11-venv"
    exit 1
fi

# Create directories
echo ""
echo "Creating directories..."
mkdir -p "${INSTALL_DIR}"
mkdir -p "${CONFIG_DIR}/certs"
mkdir -p "${DATA_DIR}/logs/flights"
mkdir -p "${DATA_DIR}/scripts"
mkdir -p "${DATA_DIR}/recordings"

# Create venv
echo "Creating Python virtual environment..."
"$PYTHON" -m venv "${VENV_DIR}"

# Install package
echo "Installing ados-drone-agent..."
"${VENV_DIR}/bin/pip" install --upgrade pip
"${VENV_DIR}/bin/pip" install ados-drone-agent

# Config
if [ ! -f "${CONFIG_DIR}/config.yaml" ]; then
    echo "Copying default config..."
    if [ -f configs/config.example.yaml ]; then
        cp configs/config.example.yaml "${CONFIG_DIR}/config.yaml"
    elif [ -f /opt/ados/configs/config.example.yaml ]; then
        cp /opt/ados/configs/config.example.yaml "${CONFIG_DIR}/config.yaml"
    else
        echo "Warning: config.example.yaml not found, using defaults"
    fi
fi

# Auto-detect serial port
echo ""
echo "Detecting flight controller..."
FC_PORT=""
for pattern in /dev/ttyACM* /dev/ttyAMA* /dev/ttyUSB*; do
    for port in $pattern; do
        if [ -e "$port" ]; then
            FC_PORT="$port"
            echo "Found FC at: $FC_PORT"
            break 2
        fi
    done
done

if [ -n "$FC_PORT" ] && [ -f "${CONFIG_DIR}/config.yaml" ]; then
    # Update serial port in config
    sed -i "s|serial_port: \"\"|serial_port: \"${FC_PORT}\"|" "${CONFIG_DIR}/config.yaml" 2>/dev/null || true
fi

# Generate device UUID
DEVICE_ID=$(cat /proc/sys/kernel/random/uuid 2>/dev/null | cut -c1-8 || echo "unknown")
echo "Device ID: ${DEVICE_ID}"
if [ -f "${CONFIG_DIR}/config.yaml" ]; then
    sed -i "s|device_id: \"\"|device_id: \"${DEVICE_ID}\"|" "${CONFIG_DIR}/config.yaml" 2>/dev/null || true
fi

# Environment file
cat > "${CONFIG_DIR}/env" <<EOF
ADOS_DEVICE_ID=${DEVICE_ID}
ADOS_CONFIG=${CONFIG_DIR}/config.yaml
EOF

# Install systemd service
echo ""
echo "Installing systemd service..."
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -f "${SCRIPT_DIR}/../systemd/ados-agent.service" ]; then
    cp "${SCRIPT_DIR}/../systemd/ados-agent.service" "${SERVICE_FILE}"
else
    # Inline minimal service file
    cat > "${SERVICE_FILE}" <<SVCEOF
[Unit]
Description=ADOS Drone Agent
After=network.target
[Service]
Type=notify
ExecStart=${VENV_DIR}/bin/ados-agent
Restart=on-failure
RestartSec=5
WatchdogSec=30
StandardOutput=journal
StandardError=journal
[Install]
WantedBy=multi-user.target
SVCEOF
fi

systemctl daemon-reload
systemctl enable ados-agent

echo ""
echo "=== Installation Complete ==="
echo ""
echo "Config:  ${CONFIG_DIR}/config.yaml"
echo "Data:    ${DATA_DIR}/"
echo "Logs:    journalctl -u ados-agent -f"
echo ""
echo "Start:   sudo systemctl start ados-agent"
echo "Status:  sudo systemctl status ados-agent"
echo "CLI:     ${VENV_DIR}/bin/ados status"

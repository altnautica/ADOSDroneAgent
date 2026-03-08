#!/usr/bin/env bash
# ADOS Drone Agent — Installation Script
# Supports: macOS (dev mode), Raspberry Pi OS, Ubuntu 22.04+, Armbian
set -euo pipefail

REPO_URL="https://github.com/altnautica/ADOSDroneAgent.git"
INSTALL_DIR="/opt/ados"
CONFIG_DIR="/etc/ados"
DATA_DIR="/var/ados"
VENV_DIR="${INSTALL_DIR}/venv"
SERVICE_FILE="/etc/systemd/system/ados-agent.service"

echo "=== ADOS Drone Agent Installer ==="
echo ""

# Detect OS
OS="$(uname -s)"
ARCH="$(uname -m)"
echo "Platform: ${OS} ${ARCH}"

# --- macOS Dev Mode ---
if [ "$OS" = "Darwin" ]; then
    echo ""
    echo "macOS detected — installing in dev mode."
    echo ""

    # Check Python 3.11+
    PYTHON=""
    for py in python3.13 python3.12 python3.11 python3; do
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
        echo "Error: Python 3.11+ required."
        echo "Install with: brew install python@3.12"
        exit 1
    fi

    # Install — prefer uv, then pipx, then pip
    if command -v uv &>/dev/null; then
        echo "Installing with uv..."
        uv tool install "git+${REPO_URL}"
    elif command -v pipx &>/dev/null; then
        echo "Installing with pipx..."
        pipx install "git+${REPO_URL}"
    else
        echo "Installing with pip..."
        "$PYTHON" -m pip install --user "git+${REPO_URL}"
    fi

    echo ""
    echo "=== Installation Complete (Dev Mode) ==="
    echo ""
    echo "Run:   ados demo          # simulated drone telemetry"
    echo "       ados tui           # TUI dashboard (in another terminal)"
    echo "       ados version       # check version"
    echo ""
    echo "No systemd on macOS — use 'ados start' to run manually."
    exit 0
fi

# --- Linux Production Mode ---

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
for py in python3.13 python3.12 python3.11 python3; do
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

# Install package from git
echo "Installing ados-drone-agent..."
"${VENV_DIR}/bin/pip" install --upgrade pip
"${VENV_DIR}/bin/pip" install "git+${REPO_URL}"

# Config
if [ ! -f "${CONFIG_DIR}/config.yaml" ]; then
    echo "Generating default config..."
    "${VENV_DIR}/bin/python" -c "
from ados.core.config import ADOSConfig
import yaml
config = ADOSConfig()
with open('${CONFIG_DIR}/config.yaml', 'w') as f:
    yaml.dump(config.model_dump(), f, default_flow_style=False)
print('Config written to ${CONFIG_DIR}/config.yaml')
"
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
    sed -i "s|serial_port: ''|serial_port: '${FC_PORT}'|" "${CONFIG_DIR}/config.yaml" 2>/dev/null || true
fi

# Generate device UUID
if [ -f /proc/sys/kernel/random/uuid ]; then
    DEVICE_ID=$(cut -c1-8 /proc/sys/kernel/random/uuid)
else
    DEVICE_ID=$(python3 -c "import uuid; print(str(uuid.uuid4())[:8])")
fi
echo "Device ID: ${DEVICE_ID}"
if [ -f "${CONFIG_DIR}/config.yaml" ]; then
    sed -i "s|device_id: ''|device_id: '${DEVICE_ID}'|" "${CONFIG_DIR}/config.yaml" 2>/dev/null || true
fi

# Environment file
cat > "${CONFIG_DIR}/env" <<EOF
ADOS_DEVICE_ID=${DEVICE_ID}
ADOS_CONFIG=${CONFIG_DIR}/config.yaml
EOF

# Install systemd service
echo ""
echo "Installing systemd service..."
cat > "${SERVICE_FILE}" <<SVCEOF
[Unit]
Description=ADOS Drone Agent
After=network.target

[Service]
Type=notify
EnvironmentFile=${CONFIG_DIR}/env
ExecStart=${VENV_DIR}/bin/ados-agent
Restart=on-failure
RestartSec=5
WatchdogSec=30
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
SVCEOF

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
echo "TUI:     ${VENV_DIR}/bin/ados tui"

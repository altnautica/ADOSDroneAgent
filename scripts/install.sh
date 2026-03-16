#!/usr/bin/env bash
# =============================================================================
# ADOS Drone Agent — Installation Script
# Supports: Raspberry Pi OS (Bookworm), Ubuntu 22.04+, Armbian, macOS (dev)
# Usage: sudo ./install.sh          (install)
#        sudo ./install.sh --uninstall (remove)
# Idempotent: safe to re-run at any time.
# =============================================================================
set -euo pipefail

REPO_URL="https://github.com/altnautica/ADOSDroneAgent.git"
INSTALL_DIR="/opt/ados"
CONFIG_DIR="/etc/ados"
DATA_DIR="/var/ados"
VENV_DIR="${INSTALL_DIR}/venv"
SERVICE_NAME="ados-agent"
SERVICE_FILE="/etc/systemd/system/${SERVICE_NAME}.service"
DEVICE_ID_FILE="${CONFIG_DIR}/device-id"

# Color helpers (degrade gracefully if not a terminal)
if [ -t 1 ]; then
    GREEN='\033[0;32m'
    YELLOW='\033[0;33m'
    RED='\033[0;31m'
    BOLD='\033[1m'
    NC='\033[0m'
else
    GREEN='' YELLOW='' RED='' BOLD='' NC=''
fi

info()  { echo -e "${GREEN}[INFO]${NC}  $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
error() { echo -e "${RED}[ERROR]${NC} $*"; }

# ─── Uninstall ───────────────────────────────────────────────────────────────

do_uninstall() {
    echo ""
    echo -e "${BOLD}=== ADOS Drone Agent — Uninstall ===${NC}"
    echo ""

    # Must be root on Linux
    if [ "$(uname -s)" != "Darwin" ] && [ "$(id -u)" -ne 0 ]; then
        error "Run as root: sudo ./install.sh --uninstall"
        exit 1
    fi

    # Stop and disable systemd service
    if [ -f "${SERVICE_FILE}" ]; then
        info "Stopping and disabling ${SERVICE_NAME} service..."
        systemctl stop "${SERVICE_NAME}" 2>/dev/null || true
        systemctl disable "${SERVICE_NAME}" 2>/dev/null || true
        rm -f "${SERVICE_FILE}"
        systemctl daemon-reload
        info "Service removed."
    else
        info "No systemd service found, skipping."
    fi

    # Remove install directory (venv + cloned code)
    if [ -d "${INSTALL_DIR}" ]; then
        info "Removing ${INSTALL_DIR}..."
        rm -rf "${INSTALL_DIR}"
    fi

    # Remove data directory
    if [ -d "${DATA_DIR}" ]; then
        info "Removing ${DATA_DIR}..."
        rm -rf "${DATA_DIR}"
    fi

    # Config is kept by default — user may want to preserve it
    if [ -d "${CONFIG_DIR}" ]; then
        warn "Config directory ${CONFIG_DIR} preserved."
        warn "Remove manually if desired: sudo rm -rf ${CONFIG_DIR}"
    fi

    echo ""
    info "Uninstall complete."
    exit 0
}

# ─── Flag Parsing ────────────────────────────────────────────────────────────

PAIR_CODE=""
DRONE_NAME=""

while [ $# -gt 0 ]; do
    case "$1" in
        --uninstall)
            do_uninstall
            ;;
        --pair)
            shift
            PAIR_CODE="${1:-}"
            if [ -z "$PAIR_CODE" ]; then
                error "--pair requires a CODE argument"
                exit 1
            fi
            shift
            ;;
        --name)
            shift
            DRONE_NAME="${1:-}"
            if [ -z "$DRONE_NAME" ]; then
                error "--name requires a NAME argument"
                exit 1
            fi
            shift
            ;;
        *)
            warn "Unknown option: $1"
            shift
            ;;
    esac
done

# ─── Architecture Detection ─────────────────────────────────────────────────

detect_arch() {
    local arch
    arch="$(uname -m)"
    case "$arch" in
        aarch64|arm64)  echo "aarch64" ;;
        armv7l|armhf)   echo "armhf" ;;
        x86_64|amd64)   echo "x86_64" ;;
        *)              echo "$arch" ;;
    esac
}

# ─── OS Detection ───────────────────────────────────────────────────────────

detect_os() {
    # Returns: raspbian, ubuntu, armbian, debian, darwin, unknown
    local os_name="unknown"

    if [ "$(uname -s)" = "Darwin" ]; then
        echo "darwin"
        return
    fi

    if [ -f /etc/os-release ]; then
        . /etc/os-release
        case "${ID:-}" in
            raspbian)   os_name="raspbian" ;;
            ubuntu)     os_name="ubuntu" ;;
            armbian)    os_name="armbian" ;;
            debian)     os_name="debian" ;;
            *)          os_name="${ID:-unknown}" ;;
        esac
    fi
    echo "$os_name"
}

# ─── Python Detection ───────────────────────────────────────────────────────

find_python() {
    # Finds the best available Python 3.11+ binary
    for py in python3.13 python3.12 python3.11 python3; do
        if command -v "$py" &>/dev/null; then
            local ver major minor
            ver=$("$py" -c 'import sys; print(f"{sys.version_info.major}.{sys.version_info.minor}")')
            major=$(echo "$ver" | cut -d. -f1)
            minor=$(echo "$ver" | cut -d. -f2)
            if [ "$major" -ge 3 ] && [ "$minor" -ge 11 ]; then
                echo "$py"
                return
            fi
        fi
    done
    echo ""
}

# ─── System Dependencies (Linux) ────────────────────────────────────────────

install_system_deps() {
    info "Installing system dependencies..."
    apt-get update -qq

    # Core: Python venv, pip, dev headers for native extensions
    # libcap-dev: Linux capabilities (for low-level device access)
    # libsystemd-dev: systemd notify protocol
    # libyaml-dev: fast YAML parsing (PyYAML C extension)
    apt-get install -y -qq \
        python3-venv \
        python3-pip \
        python3-dev \
        libcap-dev \
        libsystemd-dev \
        libyaml-dev \
        build-essential \
        git \
        curl \
        avahi-daemon \
        2>/dev/null

    info "System dependencies installed."
}

# ─── Generate Device Identity ────────────────────────────────────────────────

generate_device_id() {
    # Create a stable device UUID. Once generated, never overwrite.
    if [ -f "${DEVICE_ID_FILE}" ]; then
        info "Device identity exists: $(cat "${DEVICE_ID_FILE}")"
        return
    fi

    local device_id
    if [ -f /proc/sys/kernel/random/uuid ]; then
        device_id=$(cat /proc/sys/kernel/random/uuid)
    else
        device_id=$(python3 -c "import uuid; print(uuid.uuid4())")
    fi

    echo "$device_id" > "${DEVICE_ID_FILE}"
    chmod 644 "${DEVICE_ID_FILE}"
    info "Device identity generated: ${device_id}"
}

# ─── Generate Default Config ────────────────────────────────────────────────

generate_default_config() {
    local config_file="${CONFIG_DIR}/config.yaml"

    # Idempotent: skip if config already exists
    if [ -f "$config_file" ]; then
        info "Config already exists at ${config_file}, skipping generation."
        return
    fi

    info "Generating default config at ${config_file}..."

    # Read device ID (first 8 chars for agent name)
    local device_id=""
    if [ -f "${DEVICE_ID_FILE}" ]; then
        device_id=$(cat "${DEVICE_ID_FILE}")
    fi
    local short_id="${device_id:0:8}"

    # Use custom name if provided via --name flag
    local agent_name="${DRONE_NAME:-ados-${short_id}}"

    # Auto-detect FC serial port
    local fc_port=""
    for pattern in /dev/ttyACM* /dev/ttyAMA* /dev/ttyUSB*; do
        for port in $pattern; do
            if [ -e "$port" ]; then
                fc_port="$port"
                break 2
            fi
        done
    done

    if [ -n "$fc_port" ]; then
        info "Detected flight controller at: ${fc_port}"
    fi

    cat > "$config_file" <<CFGEOF
# ADOS Drone Agent Configuration
# Generated by install.sh on $(date -Iseconds 2>/dev/null || date)
# Docs: https://docs.altnautica.com/drone-agent/config

agent:
  device_id: "${short_id}"
  name: "${agent_name}"
  tier: "auto"

mavlink:
  serial_port: "${fc_port}"
  baud_rate: 57600
  system_id: 1
  component_id: 191

logging:
  level: "info"
  max_size_mb: 50
  keep_count: 5
  flight_log_dir: "/var/ados/logs/flights"

server:
  mode: "cloud"
  telemetry_rate: 2
  heartbeat_interval: 5

scripting:
  rest_api:
    enabled: true
    host: "0.0.0.0"
    port: 8080

pairing:
  convex_url: "https://watchful-trout-699.convex.site"
  beacon_interval: 30
  heartbeat_interval: 60

discovery:
  mdns_enabled: true
CFGEOF

    chmod 644 "$config_file"
    info "Default config written."
}

# ─── Install systemd Service ────────────────────────────────────────────────

install_systemd_service() {
    info "Installing systemd service..."

    cat > "${SERVICE_FILE}" <<SVCEOF
[Unit]
Description=ADOS Drone Agent
After=network-online.target
Wants=network-online.target

[Service]
Type=notify
EnvironmentFile=-${CONFIG_DIR}/env
ExecStart=${VENV_DIR}/bin/ados-agent
Restart=on-failure
RestartSec=5
WatchdogSec=30
StandardOutput=journal
StandardError=journal
# Security hardening
NoNewPrivileges=yes
ProtectSystem=strict
ReadWritePaths=${DATA_DIR} ${CONFIG_DIR}
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
SVCEOF

    # Write environment file
    local device_id=""
    if [ -f "${DEVICE_ID_FILE}" ]; then
        device_id=$(cat "${DEVICE_ID_FILE}")
    fi

    cat > "${CONFIG_DIR}/env" <<ENVEOF
ADOS_DEVICE_ID=${device_id}
ADOS_CONFIG=${CONFIG_DIR}/config.yaml
ENVEOF

    systemctl daemon-reload
    systemctl enable "${SERVICE_NAME}"
    systemctl start "${SERVICE_NAME}"
    info "Service installed, enabled, and started."
}

# ─── Print Status Summary ───────────────────────────────────────────────────

print_status() {
    local device_id=""
    if [ -f "${DEVICE_ID_FILE}" ]; then
        device_id=$(cat "${DEVICE_ID_FILE}")
    fi

    echo ""
    echo -e "${BOLD}=== Installation Complete ===${NC}"
    echo ""
    echo "  Install dir:  ${INSTALL_DIR}"
    echo "  Config:       ${CONFIG_DIR}/config.yaml"
    echo "  Device ID:    ${DEVICE_ID_FILE}"
    echo "  Data:         ${DATA_DIR}/"
    echo "  Venv:         ${VENV_DIR}"
    echo "  Service:      ${SERVICE_NAME}"
    echo ""
    echo "  Start:        sudo systemctl start ${SERVICE_NAME}"
    echo "  Status:       sudo systemctl status ${SERVICE_NAME}"
    echo "  Logs:         journalctl -u ${SERVICE_NAME} -f"
    echo "  CLI:          ${VENV_DIR}/bin/ados status"
    echo "  TUI:          ${VENV_DIR}/bin/ados tui"
    echo "  Diagnostics:  ${VENV_DIR}/bin/ados diag"
    echo ""

    # Quick version check
    if [ -x "${VENV_DIR}/bin/ados" ]; then
        echo "  Version:      $(${VENV_DIR}/bin/ados version 2>/dev/null || echo 'unknown')"
    fi
    echo ""
}

# =============================================================================
# Main installation flow
# =============================================================================

echo ""
echo -e "${BOLD}=== ADOS Drone Agent Installer ===${NC}"
echo ""

OS_TYPE=$(detect_os)
ARCH=$(detect_arch)
info "Platform: ${OS_TYPE} ${ARCH}"

# ─── macOS Dev Mode ─────────────────────────────────────────────────────────

if [ "$OS_TYPE" = "darwin" ]; then
    info "macOS detected. Installing in dev mode."
    echo ""

    PYTHON=$(find_python)
    if [ -z "$PYTHON" ]; then
        error "Python 3.11+ required. Install with: brew install python@3.12"
        exit 1
    fi
    info "Python: ${PYTHON} ($(${PYTHON} --version 2>&1 | awk '{print $2}'))"

    # Install using uv > pipx > pip (in order of preference)
    if command -v uv &>/dev/null; then
        info "Installing with uv..."
        uv tool install "git+${REPO_URL}"
    elif command -v pipx &>/dev/null; then
        info "Installing with pipx..."
        pipx install "git+${REPO_URL}"
    else
        info "Installing with pip..."
        "$PYTHON" -m pip install --user "git+${REPO_URL}"
    fi

    echo ""
    info "Installation complete (dev mode)."
    echo ""
    echo "  Run:    ados demo         # simulated drone telemetry"
    echo "          ados tui          # TUI dashboard"
    echo "          ados diag         # system diagnostics"
    echo "          ados version      # check version"
    echo ""
    echo "  No systemd on macOS. Use 'ados start' to run manually."
    exit 0
fi

# ─── Linux Production Mode ──────────────────────────────────────────────────

# Must be root
if [ "$(id -u)" -ne 0 ]; then
    error "Run as root: sudo ./install.sh"
    exit 1
fi

# Print detected OS
if [ -f /etc/os-release ]; then
    . /etc/os-release
    info "OS: ${PRETTY_NAME:-${OS_TYPE}}"
fi

# Validate supported OS families
case "$OS_TYPE" in
    raspbian|ubuntu|armbian|debian)
        info "Supported OS detected." ;;
    *)
        warn "Untested OS '${OS_TYPE}'. Proceeding anyway, but things may break." ;;
esac

# Validate architecture
case "$ARCH" in
    aarch64|armhf|x86_64)
        info "Architecture: ${ARCH}" ;;
    *)
        warn "Unexpected architecture '${ARCH}'. Proceeding." ;;
esac

# Check or install Python
PYTHON=$(find_python)
if [ -z "$PYTHON" ]; then
    info "Python 3.11+ not found. Attempting to install..."
    apt-get update -qq
    # Try python3.12 first (available on Bookworm), then 3.11
    if apt-cache show python3.12 &>/dev/null 2>&1; then
        apt-get install -y -qq python3.12 python3.12-venv python3.12-dev 2>/dev/null
    elif apt-cache show python3.11 &>/dev/null 2>&1; then
        apt-get install -y -qq python3.11 python3.11-venv python3.11-dev 2>/dev/null
    fi
    PYTHON=$(find_python)
    if [ -z "$PYTHON" ]; then
        error "Could not install Python 3.11+. Install manually and re-run."
        exit 1
    fi
fi
info "Python: ${PYTHON} ($(${PYTHON} --version 2>&1 | awk '{print $2}'))"

# Install system dependencies
install_system_deps

# Create directory structure
info "Creating directories..."
mkdir -p "${INSTALL_DIR}"
mkdir -p "${CONFIG_DIR}/certs"
mkdir -p "${DATA_DIR}/logs/flights"
mkdir -p "${DATA_DIR}/scripts"
mkdir -p "${DATA_DIR}/recordings"

# Create or refresh the Python venv
info "Creating Python virtual environment at ${VENV_DIR}..."
"$PYTHON" -m venv "${VENV_DIR}"

# Install the agent package
info "Installing ados-drone-agent..."
"${VENV_DIR}/bin/pip" install --upgrade pip --quiet
"${VENV_DIR}/bin/pip" install "git+${REPO_URL}" --quiet

# Generate device identity (idempotent)
generate_device_id

# Generate default config (idempotent, skips if exists)
generate_default_config

# Write pairing state if --pair was provided
PAIRING_FILE="${CONFIG_DIR}/pairing.json"
if [ -n "$PAIR_CODE" ]; then
    info "Setting pairing code: ${PAIR_CODE}"
    PAIR_CODE_UPPER=$(echo "$PAIR_CODE" | tr '[:lower:]' '[:upper:]')
    cat > "$PAIRING_FILE" <<PAIREOF
{
  "pairing_code": "${PAIR_CODE_UPPER}",
  "code_created_at": $(date +%s)
}
PAIREOF
    chmod 644 "$PAIRING_FILE"
fi

# Install systemd service
install_systemd_service

# Print summary
print_status

# Print pairing code
if [ -f "$PAIRING_FILE" ]; then
    DISPLAY_CODE=$(python3 -c "import json; print(json.load(open('${PAIRING_FILE}')).get('pairing_code', '------'))" 2>/dev/null || echo "------")
    if [ "$DISPLAY_CODE" != "------" ] && [ -n "$DISPLAY_CODE" ]; then
        echo ""
        echo -e "  ${BOLD}+----------+${NC}"
        echo -e "  ${BOLD}|  ${DISPLAY_CODE}  |${NC}  Pairing Code"
        echo -e "  ${BOLD}+----------+${NC}"
        echo ""
        echo "  Enter this code in ADOS Mission Control to pair with this drone."
        echo ""
    fi
fi

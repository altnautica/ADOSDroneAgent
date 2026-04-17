#!/usr/bin/env bash
# =============================================================================
# ADOS Drone Agent — Installation Script
# Supports: Raspberry Pi OS (Bookworm), Ubuntu 22.04+, Armbian, macOS (dev)
# Usage: sudo ./install.sh [CODE]        (install + pair)
#        sudo ./install.sh --upgrade     (upgrade only)
#        sudo ./install.sh --force       (full reinstall)
#        sudo ./install.sh --uninstall   (remove)
# Idempotent: re-runs skip completed steps. --pair is a fast path (<5s).
# =============================================================================
set -euo pipefail

# DEC-107 Bug #23: prevent needrestart and debconf from interfering with the
# install script when invoked via `curl -sSL ... | sudo bash -s -- ...`.
#
# Without these settings, an apt-pulled systemd security upgrade triggers
# `needrestart` which immediately restarts ssh.service. That kills the SSH
# pipe carrying this script's stdin to bash, bash sees EOF mid-execution,
# and any file descriptors pip/sed are holding open get closed mid-write.
# Result: 0-byte systemd unit files in /etc/systemd/system/, 0-byte pydantic
# __init__.py in the venv, and a half-broken install.
#
# Fix: tell needrestart to list-only (NEEDRESTART_MODE=l), suspend prompts
# (NEEDRESTART_SUSPEND=1), and force debconf to noninteractive frontend so
# nothing tries to prompt the user. These exports are inherited by every
# apt-get and dpkg invocation in the rest of this script.
#
# Validated 2026-04-09 on Rock 5C Lite rsdk-b1: without these env vars,
# the first `curl|sudo bash` install reliably produces 0-byte files.
export NEEDRESTART_MODE=l
export NEEDRESTART_SUSPEND=1
export DEBIAN_FRONTEND=noninteractive
export DEBCONF_NOWARNINGS=yes

REPO_URL="https://github.com/altnautica/ADOSDroneAgent.git"
BRANCH_NAME=""  # DEC-106: optional feature branch for --branch flag
INSTALL_DIR="/opt/ados"
CONFIG_DIR="/etc/ados"
DATA_DIR="/var/ados"
VENV_DIR="${INSTALL_DIR}/venv"
SERVICE_NAME="ados-supervisor"
SERVICE_FILE="/etc/systemd/system/${SERVICE_NAME}.service"
LEGACY_SERVICE="ados-agent"
SYSTEMD_SRC_DIR=""  # Set at runtime to data/systemd/ relative to repo
DEVICE_ID_FILE="${CONFIG_DIR}/device-id"
CONVEX_URL="https://convex-site.altnautica.com"

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

# ─── Installation Detection ─────────────────────────────────────────────────

is_installed() {
    [ -x "${VENV_DIR}/bin/ados" ] && "${VENV_DIR}/bin/ados" version &>/dev/null
}

get_installed_version() {
    "${VENV_DIR}/bin/ados" version 2>/dev/null | awk '{print $NF}' || echo "unknown"
}

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

    # Remove global symlinks
    rm -f /usr/local/bin/ados /usr/local/bin/ados-agent /usr/local/bin/ados-supervisor
    info "Global symlinks removed."

    # Stop and disable all ADOS systemd services
    for svc_file in /etc/systemd/system/ados-*.service; do
        [ -f "$svc_file" ] || continue
        local svc_name
        svc_name=$(basename "$svc_file" .service)
        info "Stopping and disabling ${svc_name}..."
        systemctl stop "${svc_name}" 2>/dev/null || true
        systemctl disable "${svc_name}" 2>/dev/null || true
        rm -f "$svc_file"
    done
    # Also remove legacy single-service unit
    if [ -f "/etc/systemd/system/ados-agent.service" ]; then
        systemctl stop "ados-agent" 2>/dev/null || true
        systemctl disable "ados-agent" 2>/dev/null || true
        rm -f "/etc/systemd/system/ados-agent.service"
    fi
    rm -f /etc/tmpfiles.d/ados.conf
    rm -rf /run/ados
    systemctl daemon-reload
    info "All ADOS services removed."

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
DO_FORCE=false
DO_UPGRADE=false
WITH_MESH=false  # DEC-119 / MSN-035: Phase 5 distributed RX + local mesh

# Positional pairing code: first non-flag arg that looks like a 4-8 char alphanumeric code
if [ $# -gt 0 ] && [[ "$1" =~ ^[A-Za-z0-9]{4,8}$ ]]; then
    PAIR_CODE="$1"
    shift
fi

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
        --force)
            DO_FORCE=true
            shift
            ;;
        --upgrade)
            DO_UPGRADE=true
            shift
            ;;
        --branch)
            # DEC-106: install from a feature branch instead of main
            shift
            BRANCH_NAME="${1:-}"
            if [ -z "$BRANCH_NAME" ]; then
                error "--branch requires a NAME argument"
                exit 1
            fi
            shift
            ;;
        --with-mesh)
            # DEC-119 / MSN-035: install Phase 5 mesh dependencies
            # (batctl, avahi-daemon, wpasupplicant mesh backend) and
            # mark the node as mesh-capable in /etc/ados/profile.conf.
            # Safe to combine with --upgrade on an existing install.
            WITH_MESH=true
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

    # DEC-106 Bug #10/#11: hold packages known to break mid-install on Radxa
    # BSP images before touching apt. u-boot-rk2410 postinst can trigger a
    # mid-install reboot; aic8800-usb-dkms has a broken 5.0+git build that
    # fails DKMS compile and leaves apt in a half-configured state. These
    # holds are idempotent and safe on boards where the packages aren't
    # present.
    for pkg in u-boot-rk2410 aic8800-usb-dkms radxa-system-config-aic8800-usb-dkms; do
        if dpkg -l "$pkg" 2>/dev/null | grep -q "^ii"; then
            apt-mark hold "$pkg" >/dev/null 2>&1 || true
        fi
    done

    apt-get update

    # Core: Python venv, pip, dev headers for native extensions
    # libcap-dev: Linux capabilities (for low-level device access)
    # libsystemd-dev: systemd notify protocol
    # libyaml-dev: fast YAML parsing (PyYAML C extension)
    # DEC-106 Bug #1: v4l-utils (NOT v4l2-utils — wrong package name that
    # broke install on Debian Bookworm). Bug #2: no 2>/dev/null hiding apt
    # errors — let failures surface to the install log.
    apt-get install -y \
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
        ffmpeg \
        v4l-utils \
        gstreamer1.0-tools \
        gstreamer1.0-plugins-base \
        gstreamer1.0-plugins-good \
        gstreamer1.0-rtsp

    info "System dependencies installed."
}

# ─── MediaMTX Installation ─────────────────────────────────────────────────

MEDIAMTX_VERSION="1.17.1"

install_mediamtx() {
    info "Checking mediamtx..."
    if command -v mediamtx &>/dev/null; then
        info "mediamtx already installed: $(which mediamtx)"
        return 0
    fi

    local arch
    arch="$(detect_arch)"
    local mtx_arch
    case "$arch" in
        aarch64) mtx_arch="arm64" ;;
        armhf)   mtx_arch="armv7" ;;
        x86_64)  mtx_arch="amd64" ;;
        *)
            warn "Unsupported architecture for mediamtx: $arch"
            return 1
            ;;
    esac

    local url="https://github.com/bluenviron/mediamtx/releases/download/v${MEDIAMTX_VERSION}/mediamtx_v${MEDIAMTX_VERSION}_linux_${mtx_arch}.tar.gz"
    local tmp_dir
    tmp_dir="$(mktemp -d)"

    info "Downloading mediamtx v${MEDIAMTX_VERSION} for ${mtx_arch}..."
    if curl -fSL "$url" -o "$tmp_dir/mediamtx.tar.gz"; then
        tar -xzf "$tmp_dir/mediamtx.tar.gz" -C "$tmp_dir"
        install -m 755 "$tmp_dir/mediamtx" /usr/local/bin/mediamtx
        info "mediamtx installed to /usr/local/bin/mediamtx"
    else
        warn "Failed to download mediamtx — video streaming will not work"
    fi

    rm -rf "$tmp_dir"
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
    elif command -v python3 >/dev/null 2>&1; then
        device_id=$(python3 -c "import uuid; print(uuid.uuid4())")
    elif command -v openssl >/dev/null 2>&1; then
        device_id=$(openssl rand -hex 16)
    else
        device_id="$(hostname)-$(date +%s)-$$"
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
  mqtt_transport: "websockets"
  mqtt_username: "ados"
  mqtt_password: ""

security:
  api:
    cors_enabled: true

scripting:
  rest_api:
    enabled: true
    host: "0.0.0.0"
    port: 8080

pairing:
  convex_url: "${CONVEX_URL}"
  beacon_interval: 30
  heartbeat_interval: 60

discovery:
  mdns_enabled: true

# DEC-106 Bug #12/#13: video pipeline defaults. Empty cloud_relay_url means
# local mediamtx only — configure post-install when a cloud relay is ready.
video:
  mode: "auto"
  cloud_relay_url: ""
  record: false
  camera:
    width: 1280
    height: 720
    fps: 30
    codec: "h264"
    bitrate_kbps: 4000
CFGEOF

    chmod 644 "$config_file"
    info "Default config written."
}

# ─── Install systemd Service ────────────────────────────────────────────────

install_systemd_service() {
    info "Installing systemd services (multi-process architecture)..."

    # Migrate from legacy single-service if present
    if [ -f "/etc/systemd/system/ados-agent.service" ]; then
        info "Migrating from legacy ados-agent.service..."
        systemctl stop ados-agent 2>/dev/null || true
        systemctl disable ados-agent 2>/dev/null || true
        rm -f /etc/systemd/system/ados-agent.service
    fi

    # Find systemd unit source directory
    # Check: script-level var (from upgrade clone), repo clone, script-relative
    local systemd_src=""
    if [ -n "${SYSTEMD_SRC_DIR:-}" ] && [ -d "${SYSTEMD_SRC_DIR}" ]; then
        systemd_src="${SYSTEMD_SRC_DIR}"
    elif [ -d "${INSTALL_DIR}/repo/data/systemd" ]; then
        systemd_src="${INSTALL_DIR}/repo/data/systemd"
    elif [ -d "$(dirname "$0" 2>/dev/null)/../data/systemd" ] 2>/dev/null; then
        systemd_src="$(cd "$(dirname "$0")/../data/systemd" && pwd)"
    fi

    if [ -z "$systemd_src" ] || [ ! -d "$systemd_src" ]; then
        warn "No systemd unit templates found, generating supervisor unit..."
        # Fallback: generate supervisor unit directly
        cat > "/etc/systemd/system/ados-supervisor.service" <<SVCEOF
[Unit]
Description=ADOS Drone Agent Supervisor
After=network-online.target
Wants=network-online.target

[Service]
Type=notify
ExecStart=${VENV_DIR}/bin/ados-supervisor
Restart=always
RestartSec=1
WatchdogSec=30
TimeoutStartSec=60
EnvironmentFile=-${CONFIG_DIR}/env
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
ReadWritePaths=${DATA_DIR} ${CONFIG_DIR} /run/ados
StandardOutput=journal
StandardError=journal
SyslogIdentifier=ados-supervisor

[Install]
WantedBy=multi-user.target
SVCEOF
    else
        # Deploy all unit files from data/systemd/
        local count=0
        for unit_file in "${systemd_src}"/*.service; do
            [ -f "$unit_file" ] || continue
            local unit_name
            unit_name=$(basename "$unit_file")
            # Replace venv path if different from default
            sed "s|/opt/ados/venv|${VENV_DIR}|g" "$unit_file" > "/etc/systemd/system/${unit_name}"
            count=$((count + 1))
        done
        info "Deployed ${count} systemd unit files."
    fi

    # Create /run/ados for Unix sockets (tmpfiles.d for persistence across reboots)
    mkdir -p /run/ados
    chmod 755 /run/ados
    cat > /etc/tmpfiles.d/ados.conf <<TMPEOF
d /run/ados 0755 root root -
TMPEOF

    # Write environment file
    local device_id=""
    if [ -f "${DEVICE_ID_FILE}" ]; then
        device_id=$(cat "${DEVICE_ID_FILE}")
    fi

    cat > "${CONFIG_DIR}/env" <<ENVEOF
ADOS_DEVICE_ID=${device_id}
ADOS_CONFIG=${CONFIG_DIR}/config.yaml
ADOS_RUN_DIR=/run/ados
ENVEOF

    systemctl daemon-reload

    # Enable and start supervisor (it manages all other services)
    systemctl enable ados-supervisor 2>/dev/null
    systemctl restart ados-supervisor
    info "Supervisor service enabled and started."
    info "Child services will be started by the supervisor based on hardware detection and suite config."

    # MSN-028 Phase 4 Track A Wave 3: enable cross-profile Peripheral
    # Manager unit + drop the manifest drop-in directory. Runs on both
    # drone and ground-station profiles.
    enable_universal_units

    # DEC-112 / MSN-024: enable ground-station units if profile demands them.
    if [ "${ADOS_PROFILE:-drone}" = "ground_station" ] || [ "${ADOS_PROFILE:-drone}" = "ground-station" ]; then
        enable_ground_station_units
    fi
}

# Enable cross-profile systemd units. Run on every install regardless
# of the detected profile.
enable_universal_units() {
    info "Enabling cross-profile systemd units..."
    for unit in ados-peripherals.service; do
        if [ -f "/etc/systemd/system/${unit}" ]; then
            systemctl enable "${unit}" 2>/dev/null || true
        else
            warn "Unit ${unit} not deployed; skipping enable."
        fi
    done

    # Manifest drop-in directory for /etc/ados/peripherals/*.yaml.
    mkdir -p /etc/ados/peripherals
    chmod 0755 /etc/ados/peripherals
}

# ─── Ground-station Profile (DEC-112, MSN-024) ─────────────────────────────

# Resolve agent profile. Honors /etc/ados/profile.conf if present, otherwise
# tries `python -m ados.bootstrap.profile_detect` (First Violins). Falls back
# to "drone" so a missing detector never turns a drone into a ground station.
resolve_profile() {
    local profile_file="${CONFIG_DIR}/profile.conf"
    if [ -f "${profile_file}" ]; then
        # profile.conf is a trivial `profile=<name>` or single-word file
        local val
        val="$(grep -E '^profile=' "${profile_file}" 2>/dev/null | cut -d= -f2 | tr -d '[:space:]' || true)"
        if [ -z "${val}" ]; then
            val="$(tr -d '[:space:]' < "${profile_file}" || true)"
        fi
        if [ -n "${val}" ]; then
            echo "${val}"
            return 0
        fi
    fi
    if "${VENV_DIR}/bin/python" -c "import ados.bootstrap.profile_detect" 2>/dev/null; then
        local detected
        detected="$("${VENV_DIR}/bin/python" -m ados.bootstrap.profile_detect 2>/dev/null | tr -d '[:space:]' || true)"
        if [ -n "${detected}" ]; then
            mkdir -p "${CONFIG_DIR}"
            echo "profile=${detected}" > "${profile_file}"
            echo "${detected}"
            return 0
        fi
    fi
    echo "drone"
}

# Extra apt deps needed for the ground-station profile. Idempotent.
install_ground_station_deps() {
    info "Installing ground-station profile dependencies..."
    apt-get install -y \
        hostapd \
        dnsmasq \
        bluetooth \
        bluez \
        chromium-browser \
        cage || {
        # chromium-browser has different package names on Debian Bookworm
        # (chromium) and some Radxa BSPs. Fall back gracefully.
        warn "Primary ground-station deps install failed; retrying with chromium fallback."
        apt-get install -y hostapd dnsmasq bluetooth bluez cage || true
        apt-get install -y chromium || true
    }

    # Ensure dwc2 overlay + module load for USB gadget mode (Pi family).
    local cfg="/boot/firmware/config.txt"
    if [ ! -f "${cfg}" ] && [ -f "/boot/config.txt" ]; then
        cfg="/boot/config.txt"
    fi
    if [ -f "${cfg}" ]; then
        if ! grep -qE '^\s*dtoverlay=dwc2' "${cfg}"; then
            info "Appending dtoverlay=dwc2 to ${cfg}"
            printf '\n# ADOS ground-station profile: USB gadget mode\ndtoverlay=dwc2\n' >> "${cfg}"
        fi
    else
        warn "Boot config not found; skipping dtoverlay=dwc2 append."
    fi

    local cmdline="/boot/firmware/cmdline.txt"
    if [ ! -f "${cmdline}" ] && [ -f "/boot/cmdline.txt" ]; then
        cmdline="/boot/cmdline.txt"
    fi
    if [ -f "${cmdline}" ]; then
        if ! grep -q 'modules-load=dwc2' "${cmdline}"; then
            info "Appending modules-load=dwc2 to ${cmdline}"
            # cmdline.txt is single-line; append before the trailing newline
            sed -i 's/$/ modules-load=dwc2/' "${cmdline}"
        fi
    else
        warn "Boot cmdline not found; skipping modules-load=dwc2 append."
    fi

    # MSN-027 Wave C Cellos: optional modem stack. Skipped by default so
    # ground stations without cellular hardware do not pull ~80 MB of
    # ModemManager + libqmi + libmbim just to stare at them. Set
    # `ADOS_ENABLE_MODEM=1` in the install environment to opt in.
    if [ "${ADOS_ENABLE_MODEM:-0}" = "1" ]; then
        info "ADOS_ENABLE_MODEM=1 set; installing ModemManager + QMI/MBIM utilities..."
        apt-get install -y modemmanager libqmi-utils libmbim-utils || \
            warn "Modem stack install failed; ados-modem.service will run in AT fallback mode only."
    else
        info "Skipping modem stack (set ADOS_ENABLE_MODEM=1 to install modemmanager + libqmi-utils + libmbim-utils)."
    fi

    # MSN-027 Phase 4 Wave 2 Cellos: optional share_uplink firewall
    # persistence. Skipped by default because share_uplink is opt-in
    # and pulling iptables-persistent on every ground station that
    # never plans to NAT for AP clients is wasteful. Set
    # `ADOS_ENABLE_SHARE_UPLINK=1` to install iptables-persistent on
    # Debian/Raspbian. On non-Debian or buildroot images we skip the
    # apt install and let the runtime helper fall back to nftables
    # (when present) for persistence.
    if [ "${ADOS_ENABLE_SHARE_UPLINK:-0}" = "1" ]; then
        if command -v apt-get >/dev/null 2>&1; then
            info "ADOS_ENABLE_SHARE_UPLINK=1 set; installing iptables-persistent..."
            DEBIAN_FRONTEND=noninteractive \
                debconf-set-selections <<<'iptables-persistent iptables-persistent/autosave_v4 boolean true' || true
            DEBIAN_FRONTEND=noninteractive \
                debconf-set-selections <<<'iptables-persistent iptables-persistent/autosave_v6 boolean true' || true
            DEBIAN_FRONTEND=noninteractive apt-get install -y iptables iptables-persistent || \
                warn "iptables-persistent install failed; share_uplink will use nftables fallback if available."
        else
            info "Non-Debian image; skipping iptables-persistent. share_uplink helper will use nftables fallback when 'nft' is present."
        fi
    else
        info "Skipping share_uplink firewall persistence (set ADOS_ENABLE_SHARE_UPLINK=1 to install iptables-persistent on Debian)."
    fi

    # MSN-027 Wave C Cellos: NetworkManager is mandatory for the WiFi
    # client manager (nmcli backend). Enable + start if it is installed
    # but inactive. Radxa BSPs ship with it but sometimes leave it masked.
    if systemctl list-unit-files NetworkManager.service >/dev/null 2>&1; then
        if ! systemctl is-active --quiet NetworkManager.service; then
            info "Enabling and starting NetworkManager..."
            systemctl unmask NetworkManager.service 2>/dev/null || true
            systemctl enable NetworkManager.service 2>/dev/null || true
            systemctl start NetworkManager.service 2>/dev/null || \
                warn "NetworkManager start failed; WiFi client + uplink router will be degraded."
        else
            info "NetworkManager already active."
        fi
    else
        warn "NetworkManager not installed. WiFi client manager will fail. Install with: apt-get install network-manager"
    fi

    info "Ground-station deps installed."
}

# DEC-119 / MSN-035: Phase 5 mesh dependencies. Only runs when --with-mesh
# is passed. Installs batctl + avahi-daemon + wpasupplicant with mesh
# backend, writes the mesh_capable flag into /etc/ados/profile.conf,
# and leaves the node's role at `direct` so existing deployments are
# not auto-promoted into mesh mode.
install_mesh_deps() {
    info "Installing mesh (Phase 5) dependencies..."

    if command -v apt-get >/dev/null 2>&1; then
        DEBIAN_FRONTEND=noninteractive apt-get install -y \
            batctl \
            avahi-daemon \
            wpasupplicant \
            iw || {
            warn "Mesh deps install failed; ados-batman.service will not start."
        }

        # wpad-mesh-wolfssl carries the SAE (802.11s authentication)
        # backend on Raspbian/Debian. Best-effort: not every release
        # ships it. IBSS carrier fallback works without it.
        DEBIAN_FRONTEND=noninteractive apt-get install -y \
            wpasupplicant-mesh-sae 2>/dev/null || \
            DEBIAN_FRONTEND=noninteractive apt-get install -y \
            wpad-mesh-wolfssl 2>/dev/null || \
            info "802.11s SAE backend not available via apt; IBSS fallback will apply."
    else
        warn "apt-get not found; skipping mesh deps. Install batctl + avahi-daemon manually."
    fi

    # Ensure /etc/ados/ exists (may run before ground-station deps on a
    # fresh install) then flip the mesh_capable flag in profile.conf.
    mkdir -p /etc/ados
    local pc="/etc/ados/profile.conf"
    if [ -f "${pc}" ]; then
        if grep -q '^mesh_capable:' "${pc}"; then
            sed -i 's/^mesh_capable:.*/mesh_capable: true/' "${pc}"
        else
            echo "mesh_capable: true" >> "${pc}"
        fi
    else
        cat > "${pc}" <<EOF
profile: auto
mesh_capable: true
EOF
    fi

    # Ensure the mesh identity directory exists (0o755; the PSK file
    # inside stays 0o600 and is written by mesh_manager on first boot
    # for receivers or by pairing_manager for relays).
    mkdir -p /etc/ados/mesh
    chmod 755 /etc/ados/mesh

    info "Mesh capability enabled. Role stays 'direct' until set via OLED -> Mesh or 'ados gs role set <role>'."
}

# Install RTL8812AU/EU driver via DKMS. Idempotent.
install_ground_station_driver() {
    local script_path=""
    if [ -n "${FRESH_REPO_DIR:-}" ] && [ -x "${FRESH_REPO_DIR}/repo/scripts/drivers/install-rtl8812eu.sh" ]; then
        script_path="${FRESH_REPO_DIR}/repo/scripts/drivers/install-rtl8812eu.sh"
    elif [ -x "$(dirname "$0" 2>/dev/null)/drivers/install-rtl8812eu.sh" ] 2>/dev/null; then
        script_path="$(cd "$(dirname "$0")/drivers" && pwd)/install-rtl8812eu.sh"
    fi
    if [ -z "${script_path}" ] || [ ! -x "${script_path}" ]; then
        warn "RTL8812AU installer not found; skipping driver build."
        return 0
    fi
    info "Running RTL8812AU/EU DKMS installer..."
    "${script_path}" || {
        warn "RTL8812AU DKMS install failed; WFB-ng RX will not work until resolved."
        return 0
    }
}

# Enable ground-station systemd units. Safe to run on any profile; a
# no-op for drone because we branch on profile at the call site.
enable_ground_station_units() {
    info "Enabling ground-station systemd units..."

    # MSN-029 H2: install libcomposite USB gadget script + oneshot
    # composer unit. Both are gated behind ADOS_ENABLE_USB_GADGET=1
    # (default off) until founder validates on bench. The Python-side
    # ados-usb-gadget.service Manager remains in the enable list below
    # for state transitions; it no-ops when the gadget is unbound.
    if [ "${ADOS_ENABLE_USB_GADGET:-0}" = "1" ]; then
        local gadget_src=""
        if [ -n "${FRESH_REPO_DIR:-}" ] && [ -f "${FRESH_REPO_DIR}/repo/data/usb-gadget/ados-cdc-ncm-rndis.sh" ]; then
            gadget_src="${FRESH_REPO_DIR}/repo/data/usb-gadget/ados-cdc-ncm-rndis.sh"
        elif [ -f "$(dirname "$0" 2>/dev/null)/../data/usb-gadget/ados-cdc-ncm-rndis.sh" ] 2>/dev/null; then
            gadget_src="$(cd "$(dirname "$0")/../data/usb-gadget" && pwd)/ados-cdc-ncm-rndis.sh"
        fi
        if [ -n "${gadget_src}" ] && [ -f "${gadget_src}" ]; then
            install -d -m 0755 /usr/local/lib/ados/usb-gadget
            install -m 0755 "${gadget_src}" /usr/local/lib/ados/usb-gadget/ados-cdc-ncm-rndis.sh
            info "USB gadget composer script installed (ADOS_ENABLE_USB_GADGET=1)."
            # Ensure dwc2 is loaded on Pi 4B class boards so the gadget
            # subsystem has a UDC to bind to. No-op on boards that lack
            # OTG hardware.
            if ! grep -q '^dwc2' /etc/modules 2>/dev/null; then
                echo dwc2 >> /etc/modules || true
            fi
            modprobe dwc2 2>/dev/null || true
            modprobe libcomposite 2>/dev/null || true
            systemctl enable ados-usb-gadget-setup.service 2>/dev/null || true
        else
            warn "USB gadget composer script source not found; skipping (ADOS_ENABLE_USB_GADGET=1 was set)."
        fi
    fi

    for unit in \
        ados-wfb-rx.service \
        ados-mediamtx-gs.service \
        ados-usb-gadget.service \
        ados-oled.service \
        ados-buttons.service \
        ados-hostapd.service \
        ados-dnsmasq-gs.service \
        ados-setup-captive.service \
        ados-kiosk.service \
        ados-input.service \
        ados-pic.service \
        ados-uplink-router.service \
        ados-modem.service \
        ados-wifi-client.service \
        ados-ethernet.service \
        ados-cloud-relay.service; do
        if [ -f "/etc/systemd/system/${unit}" ]; then
            systemctl enable "${unit}" 2>/dev/null || true
        else
            warn "Unit ${unit} not deployed; skipping enable."
        fi
    done

    # Ensure state + config dirs exist for AP passphrase, setup sentinel, etc.
    mkdir -p /etc/ados /var/lib/ados
    chmod 0755 /etc/ados /var/lib/ados

    # Button service uses libgpiod via /dev/gpiochip0 which requires gpio group.
    # Idempotent: usermod -aG is a no-op if the user is already a member.
    if getent group gpio >/dev/null 2>&1; then
        if id ados >/dev/null 2>&1; then
            usermod -aG gpio ados || true
        fi
        if id pi >/dev/null 2>&1; then
            usermod -aG gpio pi || true
        fi
    else
        warn "gpio group not present on this system; skipping usermod -aG gpio."
    fi

    # MSN-026 Wave C Cellos: input manager + PIC arbiter need /dev/input
    # (gamepads, evdev) and Bluetooth DBus access. Add both the `ados`
    # service user and the install-time `pi` user (if present) to the
    # input, plugdev, and bluetooth groups. All three usermod calls are
    # idempotent no-ops when membership already exists.
    #
    # MSN-029 Cellos Wave 1: also add i2c so the OLED + future I2C
    # peripherals can be driven from userspace without root.
    for grp in input plugdev bluetooth i2c; do
        if ! getent group "${grp}" >/dev/null 2>&1; then
            warn "Group ${grp} not present on this system; skipping usermod -aG ${grp}."
            continue
        fi
        if id ados >/dev/null 2>&1; then
            usermod -aG "${grp}" ados || true
        fi
        if id pi >/dev/null 2>&1; then
            usermod -aG "${grp}" pi || true
        fi
    done

    # MSN-029 Cellos Wave 1: trigger udev rebuild so i2c-dev nodes pick
    # up the new group membership without requiring a reboot.
    udevadm trigger --subsystem-match=i2c-dev || true

    # Install udev rules for gamepad + joystick hot-plug recognition.
    # Rule file ships in data/udev/ and is copied to /etc/udev/rules.d/.
    local udev_src=""
    if [ -n "${FRESH_REPO_DIR:-}" ] && [ -f "${FRESH_REPO_DIR}/repo/data/udev/99-ados-input.rules" ]; then
        udev_src="${FRESH_REPO_DIR}/repo/data/udev/99-ados-input.rules"
    elif [ -f "$(dirname "$0" 2>/dev/null)/../data/udev/99-ados-input.rules" ] 2>/dev/null; then
        udev_src="$(cd "$(dirname "$0")/../data/udev" && pwd)/99-ados-input.rules"
    fi
    if [ -n "${udev_src}" ] && [ -f "${udev_src}" ]; then
        install -m 0644 "${udev_src}" "/etc/udev/rules.d/99-ados-input.rules"
        info "Input udev rules installed."
    else
        warn "Input udev rules source not found; skipping 99-ados-input.rules install."
    fi

    # Install modem hot-plug udev rule when the modem stack is enabled.
    # Gated on ADOS_ENABLE_MODEM=1 (matches the modemmanager apt install gate).
    if [ "${ADOS_ENABLE_MODEM:-0}" = "1" ]; then
        local modem_udev_src=""
        if [ -n "${FRESH_REPO_DIR:-}" ] && [ -f "${FRESH_REPO_DIR}/repo/data/udev/99-ados-modem.rules" ]; then
            modem_udev_src="${FRESH_REPO_DIR}/repo/data/udev/99-ados-modem.rules"
        elif [ -f "$(dirname "$0" 2>/dev/null)/../data/udev/99-ados-modem.rules" ] 2>/dev/null; then
            modem_udev_src="$(cd "$(dirname "$0")/../data/udev" && pwd)/99-ados-modem.rules"
        fi
        if [ -n "${modem_udev_src}" ] && [ -f "${modem_udev_src}" ]; then
            install -m 0644 "${modem_udev_src}" "/etc/udev/rules.d/99-ados-modem.rules"
            info "Modem udev rules installed."
        else
            warn "Modem udev rules source not found; skipping 99-ados-modem.rules install."
        fi
    fi

    # Single reload + trigger after all rule copies (efficient).
    udevadm control --reload 2>/dev/null || true
    udevadm trigger 2>/dev/null || true
}

# ─── Global Symlinks ──────────────────────────────────────────────────────

install_global_symlinks() {
    ln -sf "${VENV_DIR}/bin/ados" /usr/local/bin/ados
    ln -sf "${VENV_DIR}/bin/ados-agent" /usr/local/bin/ados-agent
    if [ -f "${VENV_DIR}/bin/ados-supervisor" ]; then
        ln -sf "${VENV_DIR}/bin/ados-supervisor" /usr/local/bin/ados-supervisor
    fi
    info "Global commands installed: ados, ados-agent, ados-supervisor"
}

# ─── Write Pairing State ────────────────────────────────────────────────────

write_pairing() {
    local code="$1"
    local pairing_file="${CONFIG_DIR}/pairing.json"
    local code_upper
    code_upper=$(echo "$code" | tr '[:lower:]' '[:upper:]')

    info "Setting pairing code: ${code_upper}"
    cat > "$pairing_file" <<PAIREOF
{
  "pairing_code": "${code_upper}",
  "code_created_at": $(date +%s)
}
PAIREOF
    chmod 644 "$pairing_file"
}

# ─── Print Pairing Code Box ─────────────────────────────────────────────────

print_pairing_code() {
    local pairing_file="${CONFIG_DIR}/pairing.json"
    if [ -f "$pairing_file" ]; then
        local display_code
        display_code=$(python3 -c "import json; print(json.load(open('${pairing_file}')).get('pairing_code', '------'))" 2>/dev/null || echo "------")
        if [ "$display_code" != "------" ] && [ -n "$display_code" ]; then
            echo ""
            echo -e "  ${BOLD}+----------+${NC}"
            echo -e "  ${BOLD}|  ${display_code}  |${NC}  Pairing Code"
            echo -e "  ${BOLD}+----------+${NC}"
            echo ""
            echo "  Enter this code in ADOS Mission Control to pair with this drone."
            echo "  The agent is beaconing this code to the cloud."
            echo "  If your GCS is open, pairing should complete automatically within 30 seconds."
            echo ""
        fi
    fi
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
    echo "  CLI:          ados status"
    echo "  TUI:          ados tui"
    echo "  Diagnostics:  ados diag"
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

# ─── Fast Path: Pair-only (already installed, --pair/positional code) ────────

if is_installed && [ -n "$PAIR_CODE" ] && ! $DO_FORCE; then
    info "Agent already installed ($(get_installed_version)). Fast path: updating pairing code only."
    mkdir -p "${CONFIG_DIR}"
    write_pairing "$PAIR_CODE"
    systemctl restart "${SERVICE_NAME}" 2>/dev/null || true
    print_pairing_code
    info "Done. Service restarted with new pairing code."
    exit 0
fi

# ─── Fast Path: Already installed, no flags ──────────────────────────────────

if is_installed && ! $DO_FORCE && ! $DO_UPGRADE; then
    local_ver=$(get_installed_version)

    # Ensure global symlinks exist (fixes installs from before symlink support)
    install_global_symlinks

    echo ""
    info "ADOS Drone Agent already installed (v${local_ver})."
    echo ""
    echo "  Status:    sudo systemctl status ${SERVICE_NAME}"
    echo "  CLI:       ados status"
    echo ""
    echo "  Re-run with:"
    echo "    --upgrade    Update to latest version (skip apt, skip venv rebuild)"
    echo "    --force      Full reinstall from scratch"
    echo "    --pair CODE  Update pairing code only (<5s)"
    echo "    --with-mesh  Install batctl + avahi for Phase 5 distributed RX"
    echo "    CODE         Same as --pair CODE (positional)"
    echo ""
    print_pairing_code
    exit 0
fi

# ─── Upgrade Path (skip apt, skip venv creation) ────────────────────────────

if is_installed && $DO_UPGRADE && ! $DO_FORCE; then
    info "Upgrading ADOS Drone Agent..."
    local_ver=$(get_installed_version)
    info "Current version: ${local_ver}"

    # Ensure system deps are present (ffmpeg, v4l-utils may be missing on older installs)
    info "Checking system dependencies..."
    for pkg in ffmpeg v4l-utils avahi-daemon gstreamer1.0-tools gstreamer1.0-rtsp; do
        if ! dpkg -s "$pkg" &>/dev/null; then
            info "Installing missing system dependency: ${pkg}"
            apt-get install -y -qq "$pkg" 2>/dev/null || true
        fi
    done

    # Clone repo to temp dir for pip install + systemd files + install script
    tmp_repo="$(mktemp -d)"
    info "Fetching latest source..."
    # DEC-106: honor --branch for feature-branch installs
    if [ -n "$BRANCH_NAME" ]; then
        info "Using branch: ${BRANCH_NAME}"
        git clone --depth 1 --quiet --branch "${BRANCH_NAME}" "${REPO_URL}" "${tmp_repo}/repo"
    else
        git clone --depth 1 --quiet "${REPO_URL}" "${tmp_repo}/repo"
    fi

    # Upgrade pip package from cloned source (ensures version match)
    info "Upgrading pip package..."
    "${VENV_DIR}/bin/pip" install --upgrade "${tmp_repo}/repo" --quiet

    new_ver=$(get_installed_version)
    if [ "$local_ver" = "$new_ver" ]; then
        info "Already on latest version (${new_ver})."
    else
        info "Upgraded: ${local_ver} -> ${new_ver}"
    fi

    # Ensure mediamtx is installed
    install_mediamtx

    # Update systemd service files from cloned repo
    if [ -d "${tmp_repo}/repo/data/systemd" ]; then
        SYSTEMD_SRC_DIR="${tmp_repo}/repo/data/systemd"
    fi
    install_systemd_service

    # Clean up temp repo
    rm -rf "${tmp_repo}"

    # Ensure global symlinks point to current venv
    install_global_symlinks

    # Handle pairing code if provided alongside --upgrade
    if [ -n "$PAIR_CODE" ]; then
        write_pairing "$PAIR_CODE"
    fi

    # DEC-119 / MSN-035: --with-mesh on an existing install opts into
    # Phase 5. Installs batctl + avahi and flips mesh_capable without
    # touching role (still `direct` until operator sets it).
    if [ "${WITH_MESH}" = "true" ]; then
        install_mesh_deps
    fi

    echo ""
    info "Upgrade complete."
    print_pairing_code
    exit 0
fi

# ─── Full Install (first time or --force) ───────────────────────────────────

if $DO_FORCE && is_installed; then
    info "Force reinstall requested. Removing existing venv..."
    rm -rf "${VENV_DIR}"
fi

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

# Install mediamtx for video streaming
install_mediamtx

# Create directory structure
info "Creating directories..."
mkdir -p "${INSTALL_DIR}"
mkdir -p "${CONFIG_DIR}/certs"
mkdir -p "${DATA_DIR}/logs/flights"
mkdir -p "${DATA_DIR}/scripts"
mkdir -p "${DATA_DIR}/recordings"
mkdir -p "${INSTALL_DIR}/models/vision"
mkdir -p "${DATA_DIR}/state"

# Create or refresh the Python venv
info "Creating Python virtual environment at ${VENV_DIR}..."
"$PYTHON" -m venv "${VENV_DIR}"

# Clone repo for pip install + data files (needed when piped via curl)
FRESH_REPO_DIR=""
if [ ! -d "$(dirname "$0" 2>/dev/null)/../data/systemd" ] 2>/dev/null; then
    FRESH_REPO_DIR="$(mktemp -d)"
    info "Cloning repository..."
    # DEC-106: honor --branch for feature-branch installs
    if [ -n "$BRANCH_NAME" ]; then
        info "Using branch: ${BRANCH_NAME}"
        git clone --depth 1 --quiet --branch "${BRANCH_NAME}" "${REPO_URL}" "${FRESH_REPO_DIR}/repo"
    else
        git clone --depth 1 --quiet "${REPO_URL}" "${FRESH_REPO_DIR}/repo"
    fi
    SYSTEMD_SRC_DIR="${FRESH_REPO_DIR}/repo/data/systemd"
fi

# Install the agent package
info "Installing ados-drone-agent..."
"${VENV_DIR}/bin/pip" install --upgrade pip --quiet
if [ -n "${FRESH_REPO_DIR}" ]; then
    "${VENV_DIR}/bin/pip" install "${FRESH_REPO_DIR}/repo" --quiet
else
    "${VENV_DIR}/bin/pip" install "git+${REPO_URL}" --quiet
fi

# Resolve profile (DEC-112). Ground-station profile pulls extra apt deps,
# the RTL8812AU DKMS driver, and the ground-station python extras.
ADOS_PROFILE="$(resolve_profile)"
info "Agent profile: ${ADOS_PROFILE}"

if [ "${ADOS_PROFILE}" = "ground_station" ] || [ "${ADOS_PROFILE}" = "ground-station" ]; then
    install_ground_station_deps
    install_ground_station_driver
    info "Installing ground-station Python extras..."
    if [ -n "${FRESH_REPO_DIR}" ]; then
        "${VENV_DIR}/bin/pip" install "${FRESH_REPO_DIR}/repo[ground-station]" --quiet || \
            warn "Ground-station extras install failed; continuing."
    else
        "${VENV_DIR}/bin/pip" install "ados-drone-agent[ground-station] @ git+${REPO_URL}" --quiet || \
            warn "Ground-station extras install failed; continuing."
    fi

    # DEC-119 / MSN-035: Phase 5 mesh extras. Opt-in via --with-mesh.
    if [ "${WITH_MESH}" = "true" ]; then
        install_mesh_deps
    fi
fi

# Generate device identity (idempotent)
generate_device_id

# Generate default config (idempotent, skips if exists)
generate_default_config

# Write pairing state if code was provided
if [ -n "$PAIR_CODE" ]; then
    write_pairing "$PAIR_CODE"
fi

# Install systemd service
install_systemd_service

# Clean up temp repo if we cloned one
if [ -n "${FRESH_REPO_DIR}" ]; then
    rm -rf "${FRESH_REPO_DIR}"
fi

# Install global symlinks (ados, ados-agent → /usr/local/bin/)
install_global_symlinks

# Print summary
print_status
print_pairing_code

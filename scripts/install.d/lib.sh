# shellcheck shell=bash
# =============================================================================
# lib.sh — shared helpers and constants for the install dispatcher.
#
# Sourced first by scripts/install.sh. Every other install.d/*.sh module
# may rely on the constants and helpers declared here. Variables are
# exported so that subshells spawned by sourced functions inherit them.
# =============================================================================

# Repository + filesystem layout. Constants must stay aligned with the
# agent's runtime expectations (ados.core.paths and friends).
export REPO_URL="https://github.com/altnautica/ADOSDroneAgent.git"
export INSTALL_DIR="/opt/ados"
export CONFIG_DIR="/etc/ados"
export DATA_DIR="/var/ados"
export VENV_DIR="${INSTALL_DIR}/venv"
export SERVICE_NAME="ados-supervisor"
export DEVICE_ID_FILE="${CONFIG_DIR}/device-id"
export CONVEX_URL="https://convex-site.altnautica.com"
export MEDIAMTX_VERSION="1.17.1"

# Set at runtime by the dispatcher when a fresh git clone provides a
# different systemd unit source root. Default is empty so call sites can
# detect "use repo-relative fallback".
export SYSTEMD_SRC_DIR="${SYSTEMD_SRC_DIR:-}"

# Color helpers (degrade gracefully if stdout is not a terminal).
if [ -t 1 ]; then
    GREEN='\033[0;32m'
    YELLOW='\033[0;33m'
    RED='\033[0;31m'
    BOLD='\033[1m'
    NC='\033[0m'
else
    GREEN='' YELLOW='' RED='' BOLD='' NC=''
fi
export GREEN YELLOW RED BOLD NC

info()  { echo -e "${GREEN}[INFO]${NC}  $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
error() { echo -e "${RED}[ERROR]${NC} $*"; }
export -f info warn error

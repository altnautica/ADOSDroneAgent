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

# ─── Staged progress ─────────────────────────────────────────────────────────
# A step counter + elapsed-time banner so an operator watching a foreground
# install (over SSH or console) can always tell progress from a hang.
# ADOS_STEP_TOTAL is set by the orchestrator once per flow (fresh install vs
# upgrade); ados_stage_begin increments the counter and prints the banner,
# ados_stage_end prints the per-stage elapsed time. Output is newline-based
# only (no carriage-return spinner) so it reads cleanly through a
# `curl | sudo bash` pipe, an interactive SSH TTY, and a captured log alike —
# the color codes already degrade to empty above when stdout is not a TTY.
ADOS_STEP_NUM=0
ADOS_STEP_TOTAL="${ADOS_STEP_TOTAL:-0}"
ADOS_INSTALL_START="${ADOS_INSTALL_START:-0}"
export ADOS_STEP_NUM ADOS_STEP_TOTAL ADOS_INSTALL_START
_ADOS_STAGE_LABEL=""
_ADOS_STAGE_START=0

# ados_stage_begin LABEL — bump the step counter and print a stage banner.
# Deliberately does not touch ADOS_CURRENT_STEP; that variable is the failure
# trap's step-identifier and stays under the orchestrator's control.
ados_stage_begin() {
    ADOS_STEP_NUM=$((ADOS_STEP_NUM + 1))
    _ADOS_STAGE_LABEL="$1"
    _ADOS_STAGE_START="$(date +%s)"
    echo ""
    echo -e "${BOLD}== [${ADOS_STEP_NUM}/${ADOS_STEP_TOTAL}] ${_ADOS_STAGE_LABEL} ==${NC}"
}

# ados_stage_end — print the completed banner with the stage's elapsed time.
# Only reached when the stage's work succeeded; a failure aborts under set -e
# before this runs, leaving the failure trap to attribute the abort.
ados_stage_end() {
    local elapsed=$(( $(date +%s) - _ADOS_STAGE_START ))
    printf '%bOK%b [%s/%s] %s (%dm%02ds)\n' \
        "${GREEN}" "${NC}" "${ADOS_STEP_NUM}" "${ADOS_STEP_TOTAL}" \
        "${_ADOS_STAGE_LABEL}" $((elapsed / 60)) $((elapsed % 60))
}

# ados_install_summary — final one-liner with total elapsed + stages cleared.
ados_install_summary() {
    local total=0
    if [ "${ADOS_INSTALL_START}" -gt 0 ] 2>/dev/null; then
        total=$(( $(date +%s) - ADOS_INSTALL_START ))
    fi
    echo ""
    echo -e "${BOLD}=== Install complete in $((total / 60))m$((total % 60))s — ${ADOS_STEP_NUM}/${ADOS_STEP_TOTAL} stages OK ===${NC}"
}

# ados_with_heartbeat NOTE CMD [ARGS...] — run CMD in the foreground (so its
# stdout still streams and any shell side effects + set -e behavior are
# preserved) while a background ticker prints a one-line heartbeat every 15s.
# Returns CMD's exit status. Wrap a long step that is otherwise silent (a
# --quiet pip install, a build that logs to a file) so it never looks hung.
ados_with_heartbeat() {
    local note="$1"; shift
    _ados_heartbeat_tick "${note}" &
    local _tick_pid=$!
    local _rc=0
    "$@" || _rc=$?
    kill "${_tick_pid}" 2>/dev/null || true
    wait "${_tick_pid}" 2>/dev/null || true
    return "${_rc}"
}

_ados_heartbeat_tick() {
    local note="$1" start i
    start="$(date +%s)"
    while :; do
        # Sleep in 1s increments so the kill from ados_with_heartbeat takes
        # effect within ~1s. A plain `sleep 15` would defer the signal until
        # it returned, blocking the caller's wait for a full interval.
        i=0
        while [ "$i" -lt 15 ]; do
            sleep 1
            i=$((i + 1))
        done
        echo -e "   ${YELLOW}...${NC} ${note} ($(( $(date +%s) - start ))s elapsed)"
    done
}

export -f ados_stage_begin ados_stage_end ados_install_summary
export -f ados_with_heartbeat _ados_heartbeat_tick

# Logging helpers and build-root selection for the display overlay installer.
#
# Sourced by scripts/drivers/install-display-overlay.sh; defines functions only.
# Globals (paths, board/display ids, log colours) come from the main script.
# shellcheck shell=bash
# shellcheck disable=SC2034,SC2154

info()  { echo -e "${GREEN}[lcd-overlay]${NC}  $*"; }
warn()  { echo -e "${YELLOW}[lcd-overlay]${NC}  $*"; }
error() { echo -e "${RED}[lcd-overlay]${NC}  $*" >&2; }

# Pick the first writable temp-dir candidate. The agent runs sandboxed
# under systemd (PrivateTmp + ProtectSystem) which can render /tmp
# read-only when this script is invoked as a subprocess by the wizard.
# /run/ados is the agent's tmpfs and always writable; /var/tmp is the
# distro fallback; /tmp only works when we're called from a regular
# shell. Pick whichever lands first.
_choose_build_root() {
    local candidates=(
        "/run/ados"
        "/var/tmp"
        "${TMPDIR:-/tmp}"
        "/tmp"
    )
    for c in "${candidates[@]}"; do
        if [ -d "$c" ] && [ -w "$c" ]; then
            echo "$c"
            return 0
        fi
        # Try creating it — /run/ados may not exist on a stripped image
        # but is writable once we mkdir it.
        if mkdir -p "$c" 2>/dev/null && [ -w "$c" ]; then
            echo "$c"
            return 0
        fi
    done
    echo "/tmp"
}

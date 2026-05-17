# shellcheck shell=bash
# =============================================================================
# 04-dkms.sh — DKMS driver installers.
#
# Today this only carries install_ground_station_driver, which hands off
# to scripts/drivers/install-rtl8812eu.sh. RTL8812EU is the canonical
# wfb-ng adapter for both air and ground sides, so this runs on the
# drone profile too (the ground-station name is historical).
# =============================================================================

# Install RTL8812EU driver via DKMS. Idempotent.
install_ground_station_driver() {
    local script_path=""
    if [ -n "${FRESH_REPO_DIR:-}" ] && [ -x "${FRESH_REPO_DIR}/repo/scripts/drivers/install-rtl8812eu.sh" ]; then
        script_path="${FRESH_REPO_DIR}/repo/scripts/drivers/install-rtl8812eu.sh"
    elif [ -x "$(dirname "$0" 2>/dev/null)/drivers/install-rtl8812eu.sh" ] 2>/dev/null; then
        script_path="$(cd "$(dirname "$0")/drivers" && pwd)/install-rtl8812eu.sh"
    elif [ -x /opt/ados/source/scripts/drivers/install-rtl8812eu.sh ]; then
        # Persisted path written by persist_repo_artifacts on the
        # previous install. Lets `install.sh --upgrade` find the
        # driver script cleanly when invoked outside a fresh git
        # clone and when FRESH_REPO_DIR is not set in the calling
        # block (the upgrade path's RTL8812EU catch-up call does
        # not export FRESH_REPO_DIR, so without this fallback every
        # --upgrade silently logged "RTL8812EU installer not found;
        # skipping driver build" and the adapter never got its DKMS
        # module).
        script_path="/opt/ados/source/scripts/drivers/install-rtl8812eu.sh"
    fi
    if [ -z "${script_path}" ] || [ ! -x "${script_path}" ]; then
        warn "RTL8812EU installer not found; skipping driver build."
        return 0
    fi
    info "Running RTL8812EU DKMS installer..."
    "${script_path}" || {
        warn "RTL8812EU DKMS install failed; WFB-ng RX will not work until resolved."
        return 0
    }
}

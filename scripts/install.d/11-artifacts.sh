# shellcheck shell=bash
# =============================================================================
# 11-artifacts.sh — persist driver scripts + overlays + vendor sources.
#
# Copies scripts/drivers/, scripts/lib/, data/overlays/, vendor/rtl8812eu/,
# and data/driver-patches/ from a freshly-cloned tree into /opt/ados/source/.
# Lets the agent re-invoke driver scripts long after install.sh's temp
# repo has been cleaned up.
# =============================================================================

# Persist driver scripts and overlay sources from the cloned repo into a
# stable system path so the running agent can re-invoke them later (the
# wizard's "Local display" step in particular needs install-display-overlay.sh
# at runtime). Without this step the temp repo gets deleted at the end of
# install.sh and the agent has no way to compile or activate a DT overlay
# without another curl-pipe install. Idempotent: the install -m calls
# overwrite the targets cleanly on every run.
persist_repo_artifacts() {
    local src_root=""
    if [ -n "${FRESH_REPO_DIR:-}" ] && [ -d "${FRESH_REPO_DIR}/repo/scripts/drivers" ]; then
        src_root="${FRESH_REPO_DIR}/repo"
    elif [ -d "$(dirname "$0" 2>/dev/null)/.." ]; then
        # Dev path — running install.sh from a checked-out repo. Only use
        # this when we can resolve the parent absolutely.
        local maybe_root
        maybe_root="$(cd "$(dirname "$0")/.." 2>/dev/null && pwd)"
        if [ -n "${maybe_root}" ] && [ -d "${maybe_root}/scripts/drivers" ]; then
            src_root="${maybe_root}"
        fi
    fi
    if [ -z "${src_root}" ]; then
        warn "Cannot locate source tree to persist driver scripts; skipping."
        return 0
    fi

    local persist_root="/opt/ados/source"
    info "Persisting driver scripts and overlays to ${persist_root}/"
    install -d -m 0755 "${persist_root}/scripts/drivers"
    install -d -m 0755 "${persist_root}/scripts/lib"
    install -d -m 0755 "${persist_root}/scripts/plugin-keys"
    install -d -m 0755 "${persist_root}/scripts/peripherals-seed"
    install -d -m 0755 "${persist_root}/data/overlays/upstream"

    # First-party plugin signing public keys. These ride along with the
    # repo and get re-deployed on every upgrade so provision_plugin_keys
    # can populate /etc/ados/plugin-keys/ even when install.sh runs via
    # curl-pipe with no checked-out tree.
    if [ -d "${src_root}/scripts/plugin-keys" ]; then
        find "${src_root}/scripts/plugin-keys" -maxdepth 1 -type f -name '*.pem' -print0 \
            | while IFS= read -r -d '' f; do
                install -m 0644 "$f" "${persist_root}/scripts/plugin-keys/"
            done
    fi

    # First-party peripheral manifests for the BOM fleet (FC, GPS,
    # RTL8812EU adapter, OLED, SPI LCD, USB camera). seed_default_peripherals
    # reads from this persisted path so /etc/ados/peripherals/*.yaml is
    # populated even when install.sh runs via curl-pipe.
    if [ -d "${src_root}/scripts/peripherals-seed" ]; then
        find "${src_root}/scripts/peripherals-seed" -maxdepth 1 -type f -name '*.yaml' -print0 \
            | while IFS= read -r -d '' f; do
                install -m 0644 "$f" "${persist_root}/scripts/peripherals-seed/"
            done
    fi

    # Driver shell scripts (install-display-overlay.sh, install-rtl8812eu.sh, ...)
    if [ -d "${src_root}/scripts/drivers" ]; then
        find "${src_root}/scripts/drivers" -maxdepth 1 -type f -name '*.sh' -print0 \
            | while IFS= read -r -d '' f; do
                install -m 0755 "$f" "${persist_root}/scripts/drivers/"
            done
    fi

    # Shared shell helpers sourced by driver scripts at runtime
    # (display-conf-helpers.sh today; more to come). Persisted as 0644
    # because they're library files that get sourced, not executed.
    if [ -d "${src_root}/scripts/lib" ]; then
        find "${src_root}/scripts/lib" -maxdepth 1 -type f -name '*.sh' -print0 \
            | while IFS= read -r -d '' f; do
                install -m 0644 "$f" "${persist_root}/scripts/lib/"
            done
    fi

    # Repo-shipped device-tree overlay sources (compiled at install time).
    if [ -d "${src_root}/data/overlays" ]; then
        find "${src_root}/data/overlays" -maxdepth 1 -type f \( -name '*.dts' -o -name '*.dtsi' \) -print0 \
            | while IFS= read -r -d '' f; do
                install -m 0644 "$f" "${persist_root}/data/overlays/"
            done
    fi

    # Vendored upstream overlay sources used as a fallback when the BSP
    # overlay package is absent.
    if [ -d "${src_root}/data/overlays/upstream" ]; then
        find "${src_root}/data/overlays/upstream" -maxdepth 1 -type f -print0 \
            | while IFS= read -r -d '' f; do
                install -m 0644 "$f" "${persist_root}/data/overlays/upstream/"
            done
    fi

    # Vendored RTL8812EU kernel source (a git submodule under
    # vendor/rtl8812eu). install-rtl8812eu.sh resolves the source via
    # SCRIPT_DIR/../../vendor/rtl8812eu, so when it runs from the
    # persisted path /opt/ados/source/scripts/drivers/ it expects the
    # tree at /opt/ados/source/vendor/rtl8812eu. Without this copy
    # every `install.sh --upgrade` re-run silently logged
    # "Vendor source not found at /opt/ados/source/vendor/rtl8812eu"
    # and the DKMS module never rebuilt, leaving the WFB-ng radio
    # adapter dead at the kernel layer.
    if [ -d "${src_root}/vendor/rtl8812eu" ]; then
        install -d -m 0755 "${persist_root}/vendor"
        rm -rf "${persist_root}/vendor/rtl8812eu"
        cp -a "${src_root}/vendor/rtl8812eu" "${persist_root}/vendor/"
    fi

    # Driver-patch tree (mesh-enable patch + future build-time
    # adjustments) the rtl8812eu installer reads via REPO_ROOT/data/
    # driver-patches/.
    if [ -d "${src_root}/data/driver-patches" ]; then
        install -d -m 0755 "${persist_root}/data/driver-patches"
        find "${src_root}/data/driver-patches" -maxdepth 1 -type f -print0 \
            | while IFS= read -r -d '' f; do
                install -m 0644 "$f" "${persist_root}/data/driver-patches/"
            done
    fi

    info "Driver scripts + overlay sources persisted (drivers + lib + overlays + upstream + vendor/rtl8812eu + driver-patches)."
}

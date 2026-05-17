# shellcheck shell=bash
# =============================================================================
# 01-state.sh — installed-state probes, uninstall path, global symlinks.
#
# The dispatcher's main flow routes through is_installed/get_installed_version
# to choose between fast-path / upgrade / full install. do_uninstall is the
# `--uninstall` entry point and intentionally exits 0 when complete.
# =============================================================================

is_installed() {
    # The CLI's `ados version` subcommand does not exist; checking the
    # CLI binary's presence is enough to know the agent has been
    # deployed to /opt/ados. The Python package always ships an
    # __init__.py with the version constant.
    [ -x "${VENV_DIR}/bin/ados" ] && [ -d "${VENV_DIR}/lib" ]
}

get_installed_version() {
    # Read the version straight from the package's __init__.py rather
    # than going through the CLI, which has no version subcommand.
    "${VENV_DIR}/bin/python" -c "import ados; print(ados.__version__)" 2>/dev/null || echo "unknown"
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
    rm -f /etc/sysctl.d/99-ados-video.conf
    rm -rf /run/ados
    rm -f /etc/update-motd.d/30-ados
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

# ─── Global Symlinks ──────────────────────────────────────────────────────

install_global_symlinks() {
    ln -sf "${VENV_DIR}/bin/ados" /usr/local/bin/ados
    ln -sf "${VENV_DIR}/bin/ados-agent" /usr/local/bin/ados-agent
    if [ -f "${VENV_DIR}/bin/ados-supervisor" ]; then
        ln -sf "${VENV_DIR}/bin/ados-supervisor" /usr/local/bin/ados-supervisor
    fi
    info "Global commands installed: ados, ados-agent, ados-supervisor"
}

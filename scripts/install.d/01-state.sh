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

# ─── State directory bootstrap ───────────────────────────────────────────────
#
# Creates the long-lived state + log + runtime dirs that systemd units
# reference via ReadWritePaths and that the agent writes into. Called
# from main_install_flow before any unit is deployed.
#
# /var/lib/ados — agent state (pairing token, device-id, AP passphrase)
# /var/log/ados — supervisor + plugin logs
# /run/ados     — IPC sockets (also created by tmpfiles.d on boot)
#
# Idempotent. `install -d` is a no-op if the dir already exists with the
# right mode/owner; the chown/chmod are explicit so they self-heal a
# previously-misconfigured deployment.
setup_state_dirs() {
    install -d -m 0755 -o root -g root /var/lib/ados
    install -d -m 0755 -o root -g root /var/log/ados
    install -d -m 0755 -o root -g root /run/ados
}

# ─── Stale-state detection ───────────────────────────────────────────────────
#
# Returns 0 when the system has residue from an incomplete prior uninstall:
# unit files, dropin dirs, or top-level agent dirs that survived while the
# venv binary is gone. Caller (main_install_flow) auto-purges via
# do_uninstall and continues, so a partial-prior-state never silently
# blocks a fresh install.
detect_stale_state() {
    # Already a clean install — not stale, fast-path will handle it.
    if is_installed; then
        return 1
    fi
    # Service or slice unit files surviving past venv removal.
    if compgen -G "/etc/systemd/system/ados-*.service" >/dev/null; then
        return 0
    fi
    if compgen -G "/etc/systemd/system/ados-*.slice" >/dev/null; then
        return 0
    fi
    if [ -d /etc/systemd/system/ados-supervisor.service.wants ]; then
        return 0
    fi
    # Top-level dirs without a usable venv inside.
    if [ -d /opt/ados ] || [ -d /etc/ados ] || [ -d /var/lib/ados ]; then
        return 0
    fi
    return 1
}

get_installed_version() {
    # Read the version straight from the package's __init__.py rather
    # than going through the CLI, which has no version subcommand.
    "${VENV_DIR}/bin/python" -c "import ados; print(ados.__version__)" 2>/dev/null || echo "unknown"
}

# ─── Uninstall ───────────────────────────────────────────────────────────────

# ─── Cleanup primitive ───────────────────────────────────────────────────────
#
# Removes every artifact this install lays down outside of /opt/ados.
# Used by both do_uninstall (which prints a header and exits 0 when done)
# and by the stale-state auto-purge path in main_install_flow (which must
# return to its caller so the fresh install can proceed). Does not exit.
purge_ados_artifacts() {
    # Remove global symlinks
    rm -f /usr/local/bin/ados /usr/local/bin/ados-agent /usr/local/bin/ados-supervisor

    # Stop and disable all ADOS systemd services
    for svc_file in /etc/systemd/system/ados-*.service; do
        [ -f "$svc_file" ] || continue
        local svc_name
        svc_name=$(basename "$svc_file" .service)
        systemctl stop "${svc_name}" 2>/dev/null || true
        systemctl disable "${svc_name}" 2>/dev/null || true
        rm -f "$svc_file"
    done
    # Slice + target + timer units (plugin slice is the main one today).
    for unit_glob in "/etc/systemd/system/ados-*.slice" \
                     "/etc/systemd/system/ados-*.target" \
                     "/etc/systemd/system/ados-*.timer"; do
        for unit_file in $unit_glob; do
            [ -f "$unit_file" ] || continue
            systemctl stop "$(basename "$unit_file")" 2>/dev/null || true
            rm -f "$unit_file"
        done
    done
    # Dropin .wants directories (supervisor builds these to pull in
    # profile-specific children) and orphan multi-user.target.wants symlinks.
    rm -rf /etc/systemd/system/ados-*.service.wants
    rm -f /etc/systemd/system/multi-user.target.wants/ados-*
    # Also remove legacy single-service unit
    if [ -f "/etc/systemd/system/ados-agent.service" ]; then
        systemctl stop "ados-agent" 2>/dev/null || true
        systemctl disable "ados-agent" 2>/dev/null || true
        rm -f "/etc/systemd/system/ados-agent.service"
    fi
    # Tmpfiles, sysctl, modules-load, udev, avahi, motd dropins.
    rm -f /etc/tmpfiles.d/ados.conf
    rm -f /etc/tmpfiles.d/ados-plugins.conf
    rm -f /etc/sysctl.d/99-ados-video.conf
    rm -f /etc/modules-load.d/ados-display.conf
    rm -f /etc/udev/rules.d/50-ados-uvc-no-autosuspend.rules
    rm -f /etc/udev/rules.d/99-ados-hardware.rules
    rm -f /etc/udev/rules.d/99-ados-input.rules
    rm -f /etc/udev/rules.d/99-ados-modem.rules
    rm -f /etc/avahi/services/ados-gs-ap.service
    rm -f /etc/update-motd.d/30-ados

    # Power hardening artifacts. Drop the NetworkManager / udev / logind
    # drop-ins, unmask the sleep targets, and tear down the re-assert
    # oneshot so the box returns to its stock power behavior.
    rm -f /etc/NetworkManager/conf.d/99-ados-wifi-powersave.conf
    rm -f /etc/udev/rules.d/99-ados-wifi-powersave.rules
    rm -f /etc/udev/rules.d/99-ados-usb-no-autosuspend.rules
    rm -f /etc/udev/rules.d/99-ados-eth-no-eee.rules
    rm -f /etc/systemd/logind.conf.d/99-ados-nosleep.conf
    systemctl unmask sleep.target suspend.target hibernate.target \
        hybrid-sleep.target suspend-then-hibernate.target 2>/dev/null || true
    systemctl disable ados-power.service 2>/dev/null || true
    rm -f /etc/systemd/system/ados-power.service
    rm -f /opt/ados/bin/ados-power-reassert.sh

    rm -rf /run/ados
    rm -rf /var/log/ados
    systemctl daemon-reload 2>/dev/null || true
    systemctl reset-failed 2>/dev/null || true
    udevadm control --reload-rules 2>/dev/null || true

    # Remove install + data directories. INSTALL_DIR holds the venv + cloned
    # code; DATA_DIR is the legacy data path.
    if [ -d "${INSTALL_DIR}" ]; then
        rm -rf "${INSTALL_DIR}"
    fi
    if [ -d "${DATA_DIR}" ]; then
        rm -rf "${DATA_DIR}"
    fi
}

do_uninstall() {
    echo ""
    echo -e "${BOLD}=== ADOS Drone Agent — Uninstall ===${NC}"
    echo ""

    # Must be root on Linux
    if [ "$(uname -s)" != "Darwin" ] && [ "$(id -u)" -ne 0 ]; then
        error "Run as root: sudo ./install.sh --uninstall"
        exit 1
    fi

    purge_ados_artifacts
    info "All ADOS services, dropins, and state removed."

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
    # Fetch the terminal dashboard binary that `ados` (no subcommand) launches.
    # Best-effort; `ados` falls back to plain status if it is not present.
    install_tui_binary
    # Fetch the prebuilt orchestrator binary onto disk. Best-effort and inert:
    # the systemd unit decides which ExecStart to run, so a present binary is a
    # no-op until a unit points at it.
    install_supervisor_binary
    # Fetch the prebuilt MAVLink router binary. The ados-mavlink unit's ExecStart
    # shim runs it when present, else the packaged Python service.
    install_mavlink_router_binary
    # Fetch the prebuilt WFB radio binary. The ados-wfb unit's ExecStart shim
    # runs it when present (drone profile), else the packaged Python service.
    install_radio_binary
    # Fetch the prebuilt video orchestrator binary. The ados-video unit's
    # ExecStart shim runs it when present AND the operator opted in via the
    # flag, else the packaged Python service.
    install_video_binary
    # Fetch the prebuilt plugin-host binary. The ados-plugin-host unit is a new
    # opt-in service shipped disabled; its ExecStart shim runs the binary only
    # when present AND the operator enabled the flag, else /bin/true.
    install_plugin_host_binary
    # Fetch the prebuilt cloud-relay binaries (ados-cloud + ados-ota). The
    # ados-cloud unit's ExecStart shim runs the native binary when present AND
    # the operator enabled the flag, else the packaged Python service.
    install_cloud_binary
}

# shellcheck shell=bash
# =============================================================================
# 03-kernel.sh — kernel-side tunables (sysctl) and display overlay handoff.
#
# install_video_sysctl writes the UDP buffer ceiling for the wfb_rx +
# fanout + mediamtx ingest chain. install_display_driver hands off to
# the persisted overlay installer script for SPI LCDs.
# =============================================================================

# Drop a sysctl tuning file that bumps the kernel UDP socket buffer
# ceiling so the wfb_rx + fanout + mediamtx ingest chain can absorb
# bursty 802.11 frame deliveries without dropping. The video_fanout
# socket already requests 4 MiB SO_RCVBUF/SO_SNDBUF; on a stock
# Debian/Ubuntu kernel net.core.rmem_max is 208 KiB so the request
# is silently clamped. Bumping the ceiling to 16 MiB lets the
# requested allocation actually land.
#
# This is a well-trodden tuning for any high-throughput UDP receiver
# and matches the values mediamtx, gstreamer, and ffmpeg recommend in
# their own production-tuning docs. Idempotent: writes a drop-in,
# applies once via sysctl --system fallback. Removed cleanly by the
# uninstall path below.
install_video_sysctl() {
    info "Installing video sysctl tuning..."
    cat > /etc/sysctl.d/99-ados-video.conf <<'SYSCTLEOF'
# ADOS video pipeline UDP buffer ceiling. Allows the wfb_rx +
# video_fanout + mediamtx UDP sockets to actually allocate the
# 4 MiB SO_RCVBUF / SO_SNDBUF they request at bind time. Without
# this, the kernel silently clamps to net.core.rmem_max ~208 KiB
# and bursty FEC frame deliveries drop packets at the kernel.
net.core.rmem_max = 16777216
net.core.wmem_max = 16777216
net.core.rmem_default = 4194304
net.core.wmem_default = 4194304
SYSCTLEOF
    chmod 0644 /etc/sysctl.d/99-ados-video.conf
    # Apply now so the running agent picks up the new ceiling on
    # the next socket bind. Suppressed if the host lacks sysctl
    # (e.g. inside a stripped container build path).
    if command -v sysctl >/dev/null 2>&1; then
        sysctl -p /etc/sysctl.d/99-ados-video.conf >/dev/null 2>&1 || true
    fi
}

# SPI LCD overlay installer for boards that ship displays.supported in
# their YAML profile. Compiles or activates the right device-tree
# overlay, writes /etc/ados/display.conf, and queues fbtft + ads7846
# for next boot. Operator can override with ADOS_DISPLAY=<id> or
# ADOS_DISPLAY=none. Failure is non-fatal so a missing LCD or missing
# kernel headers does not block the rest of the install.
install_display_driver() {
    local script_path=""
    if [ -n "${FRESH_REPO_DIR:-}" ] && [ -x "${FRESH_REPO_DIR}/repo/scripts/drivers/install-display-overlay.sh" ]; then
        script_path="${FRESH_REPO_DIR}/repo/scripts/drivers/install-display-overlay.sh"
    elif [ -x "$(dirname "$0" 2>/dev/null)/drivers/install-display-overlay.sh" ] 2>/dev/null; then
        script_path="$(cd "$(dirname "$0")/drivers" && pwd)/install-display-overlay.sh"
    elif [ -x /opt/ados/source/scripts/drivers/install-display-overlay.sh ]; then
        # Persisted path — present after persist_repo_artifacts has run at
        # least once. Lets `install.sh --upgrade` run the LCD overlay step
        # cleanly even when invoked outside a fresh git clone.
        script_path="/opt/ados/source/scripts/drivers/install-display-overlay.sh"
    fi
    if [ -z "${script_path}" ] || [ ! -x "${script_path}" ]; then
        warn "LCD overlay installer not found; skipping display provisioning."
        return 0
    fi

    # Resolve the display selection. An explicit ADOS_DISPLAY=<id> (or
    # ADOS_DISPLAY=none) from the operator always wins. With no explicit
    # value the default is profile-aware:
    #
    #   * drone / lite — default to "none". A headless flight rig has no
    #     panel attached. The auto-pickable display on Rockchip boards is
    #     a boot-critical SPI-LCD overlay (it rewrites the board's only
    #     boot config via u-boot-update and queues a framebuffer driver
    #     to load every boot). Provisioning that against absent hardware
    #     leaves the box unbootable, and it re-applies on every boot so a
    #     power cycle cannot recover it. A drone must never auto-provision
    #     one.
    #
    #   * ground-station — default to "auto". A ground unit usually does
    #     carry a status panel, so it keeps the auto-detect behaviour.
    #     (The overlay script's own auto path is also defensive and will
    #     not silently apply a boot-critical SPI-LCD when only that type
    #     is on offer — see scripts/drivers/install-display-overlay.sh.)
    local display_arg="${ADOS_DISPLAY:-}"
    if [ -z "${display_arg}" ]; then
        case "${ADOS_PROFILE:-drone}" in
            drone|lite|lite-rs|lite_rs)
                display_arg="none"
                ;;
            ground_station|ground-station)
                display_arg="auto"
                ;;
            *)
                display_arg="none"
                ;;
        esac
    fi

    info "Running LCD overlay installer (display: ${display_arg})..."
    "${script_path}" --display "${display_arg}" || {
        warn "LCD overlay install returned non-zero; the agent will boot without an attached panel."
        return 0
    }
}

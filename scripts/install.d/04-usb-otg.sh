# shellcheck shell=bash
# =============================================================================
# 04-usb-otg.sh — force USB OTG controllers into host mode.
#
# Some single-board computers default their USB-C / OTG controller to the
# USB device (peripheral) role at boot. In that role a connected powered hub's
# downstream peripherals (USB WiFi radio adapter, cameras) never enumerate.
# Writing usb_host to the controller's sysfs otg_role node switches it to host
# mode. provision_usb_otg_host applies the role immediately for the current
# session and installs a boot-time oneshot unit so the role survives reboots.
#
# No-op on boards that expose no such node (most Rockchip / Pi boards), so it
# is safe to call unconditionally on every install and upgrade.
# =============================================================================

# Glob pattern set covering the controller's otg_role node across the layouts
# seen on Allwinner BSPs: the platform-device bus path and the soc-rooted
# device path. The Type-C data_role node does NOT switch the controller; the
# OTG controller's own otg_role does.
_ados_otg_role_globs() {
    printf '%s\n' \
        /sys/bus/platform/devices/*.usbc*/otg_role \
        /sys/devices/platform/soc*/*.usbc*/otg_role
}

provision_usb_otg_host() {
    local switched=0 found=0 node cur

    # Apply now for the current session so a hub plugged in during install
    # enumerates without a reboot.
    while IFS= read -r node; do
        # Skip the literal glob when nothing matched.
        [ -e "${node}" ] || continue
        found=$((found + 1))
        cur="$(cat "${node}" 2>/dev/null || true)"
        if [ "${cur}" = "usb_device" ]; then
            if echo usb_host > "${node}" 2>/dev/null; then
                info "Switched ${node} to host mode (was usb_device)."
                switched=$((switched + 1))
            else
                warn "Could not write usb_host to ${node}."
            fi
        fi
    done < <(_ados_otg_role_globs)

    if [ "${found}" -eq 0 ]; then
        # No OTG role node on this board — nothing to provision and no boot
        # unit needed. Common case on Rockchip / Pi hardware.
        return 0
    fi

    if [ "${switched}" -eq 0 ]; then
        info "USB OTG controller(s) already in host mode."
    fi

    # Install the boot-time oneshot unit so the host role is re-applied on
    # every boot. The unit file ships in data/systemd/ and is deployed by
    # install_systemd_service; we only ensure it's enabled here. Enabling is
    # idempotent. Fall back to writing the unit inline when the repo copy is
    # not yet on disk (e.g. a pair-only fast path that skipped unit deploy).
    if [ ! -f /etc/systemd/system/ados-usb-otg-host.service ]; then
        _write_usb_otg_host_unit
        systemctl daemon-reload >/dev/null 2>&1 || true
    fi
    if [ -f /etc/systemd/system/ados-usb-otg-host.service ]; then
        systemctl enable ados-usb-otg-host.service >/dev/null 2>&1 \
            || warn "Could not enable ados-usb-otg-host.service; host mode may not survive reboot."
    fi
}

# Inline fallback writer for the boot unit. Kept byte-identical in intent to
# data/systemd/ados-usb-otg-host.service so a unit written here matches the
# repo copy a later upgrade deploys.
_write_usb_otg_host_unit() {
    cat > /etc/systemd/system/ados-usb-otg-host.service <<'OTGEOF'
[Unit]
Description=ADOS force USB OTG controllers into host mode
DefaultDependencies=no
After=systemd-modules-load.service
Before=ados-supervisor.service network-pre.target
Wants=network-pre.target

[Service]
Type=oneshot
RemainAfterExit=yes
TimeoutStartSec=45
ExecStart=/bin/sh -c 'grep -qiE "allwinner|sunxi" /proc/device-tree/compatible 2>/dev/null || exit 0; i=0; while [ "$i" -lt 30 ]; do pending=0; any=0; for n in /sys/bus/platform/devices/*.usbc*/otg_role /sys/devices/platform/soc*/*.usbc*/otg_role; do [ -e "$n" ] || continue; any=1; cur=$(cat "$n" 2>/dev/null || echo); [ "$cur" = usb_device ] && echo usb_host > "$n" 2>/dev/null; cur=$(cat "$n" 2>/dev/null || echo); [ "$cur" = usb_host ] || pending=1; done; { [ "$any" = 1 ] && [ "$pending" = 0 ]; } && exit 0; i=$((i + 1)); sleep 1; done; exit 0'
StandardOutput=journal
StandardError=journal
SyslogIdentifier=ados-usb-otg-host

[Install]
WantedBy=multi-user.target
OTGEOF
}

# shellcheck shell=bash
# =============================================================================
# 03b-power.sh — board-agnostic power hardening.
#
# install_power_hardening keeps the radios, the management WiFi, and the
# camera continuously powered by disabling every common power-down path:
#   - WiFi power-save (durable NetworkManager drop-in + fallback udev rule)
#   - USB autosuspend (broad udev rule covering radio dongles + modem)
#   - Ethernet Energy-Efficient-Ethernet (EEE) on the PHY
#   - system sleep / suspend / hibernate targets (masked)
#   - logind idle / lid / power-key actions (ignored)
#   - a boot oneshot that re-asserts the runtime knobs after a cold boot
#
# Every rule matches generically (wlan*, usb, eth*/end*/enP*/enx*) and
# no-ops where the device or knob is absent, so there is no per-board
# code. Idempotent: each call overwrites its own drop-ins and is safe to
# re-run on every install + upgrade. Reversible by purge_ados_artifacts.
# CPU governor is intentionally left untouched.
# =============================================================================

# Prefix an absolute filesystem path with ADOS_FS_ROOT. Empty in
# production (the real filesystem); set by the test harness to a temp
# tree so the file writes can be asserted without root. The directory
# the target lives in is created on demand by the callers.
_power_path() {
    printf '%s\n' "${ADOS_FS_ROOT:-}$1"
}

# Resolve a data/ source file (udev rule, systemd unit) from either the
# fresh-clone tree or the script-relative install.d/../data path. Echoes
# the resolved absolute path on stdout, or nothing when not found. Mirrors
# the resolution pattern used by install_systemd_service.
_power_resolve_data_file() {
    local rel="$1"  # e.g. udev/99-ados-wifi-powersave.rules
    if [ -n "${FRESH_REPO_DIR:-}" ] && [ -f "${FRESH_REPO_DIR}/repo/data/${rel}" ]; then
        printf '%s\n' "${FRESH_REPO_DIR}/repo/data/${rel}"
        return 0
    fi
    if [ -d "${INSTALL_DIR}/repo/data" ] && [ -f "${INSTALL_DIR}/repo/data/${rel}" ]; then
        printf '%s\n' "${INSTALL_DIR}/repo/data/${rel}"
        return 0
    fi
    local script_data
    if script_data="$(cd "$(dirname "$0" 2>/dev/null)/../data" 2>/dev/null && pwd)" \
        && [ -f "${script_data}/${rel}" ]; then
        printf '%s\n' "${script_data}/${rel}"
        return 0
    fi
    return 1
}

# Resolve the absolute path to the `iw` binary, preferring the standard
# sbin locations. Falls back to a plain `iw` lookup via the udev helper
# shell when neither path resolves.
_power_resolve_iw() {
    if [ -x /usr/sbin/iw ]; then
        printf '%s\n' /usr/sbin/iw
    elif [ -x /sbin/iw ]; then
        printf '%s\n' /sbin/iw
    elif command -v iw >/dev/null 2>&1; then
        command -v iw
    fi
}

# Resolve the absolute path to the `ethtool` binary.
_power_resolve_ethtool() {
    if [ -x /usr/sbin/ethtool ]; then
        printf '%s\n' /usr/sbin/ethtool
    elif [ -x /sbin/ethtool ]; then
        printf '%s\n' /sbin/ethtool
    elif command -v ethtool >/dev/null 2>&1; then
        command -v ethtool
    fi
}

install_power_hardening() {
    info "Hardening power management (WiFi / USB / Ethernet / sleep)..."

    # ── 1. WiFi power-save off (durable, NetworkManager) ─────────────────
    # NetworkManager defaults wifi.powersave to 3 (use the kernel/driver
    # default, which is usually ON). A drop-in pins it to 2 (force OFF) for
    # every managed WiFi connection, including the management link the GCS
    # reaches the agent over and a ground-station uplink client. Skipped
    # cleanly when NM is not installed (Buildroot / minimal rootfs).
    local nm_conf
    nm_conf="$(_power_path /etc/NetworkManager/conf.d/99-ados-wifi-powersave.conf)"
    if [ -d /etc/NetworkManager ] || command -v nmcli >/dev/null 2>&1; then
        install -d -m 0755 "$(dirname "${nm_conf}")"
        cat > "${nm_conf}" <<'NMEOF'
# ADOS: force WiFi power-save OFF for every managed connection so the
# management link and any WiFi uplink never park the radio.
# 2 = disable power save.
[connection]
wifi.powersave = 2
NMEOF
        chmod 0644 "${nm_conf}"
        # Reload NM so the drop-in takes effect without a reboot.
        if command -v nmcli >/dev/null 2>&1; then
            nmcli general reload 2>/dev/null || \
                systemctl reload NetworkManager 2>/dev/null || true
        else
            systemctl reload NetworkManager 2>/dev/null || true
        fi
    else
        info "NetworkManager not present; skipping WiFi power-save drop-in."
    fi

    # ── 2. WiFi power-save off (fallback udev) ───────────────────────────
    # Belt-and-suspenders for interfaces NM does not manage (a raw wlan*
    # brought up by the WFB stack, or a system with no NM). Fires on every
    # WiFi netdev add and disables power_save directly via iw.
    local udev_dir wifi_rule usb_rule eth_rule
    udev_dir="$(_power_path /etc/udev/rules.d)"
    install -d -m 0755 "${udev_dir}"
    wifi_rule="${udev_dir}/99-ados-wifi-powersave.rules"
    usb_rule="${udev_dir}/99-ados-usb-no-autosuspend.rules"
    eth_rule="${udev_dir}/99-ados-eth-no-eee.rules"

    local iw_bin
    iw_bin="$(_power_resolve_iw)"
    {
        echo '# ADOS: disable WiFi power-save on every wlan* interface as it appears.'
        if [ -n "${iw_bin}" ]; then
            echo "ACTION==\"add\", SUBSYSTEM==\"net\", KERNEL==\"wlan*\", RUN+=\"${iw_bin} dev %k set power_save off\""
        else
            echo 'ACTION=="add", SUBSYSTEM=="net", KERNEL=="wlan*", RUN+="/bin/sh -c '"'"'iw dev %k set power_save off'"'"'"'
        fi
    } > "${wifi_rule}"
    chmod 0644 "${wifi_rule}"

    # ── 3. USB autosuspend off (broad) ──────────────────────────────────
    # The kernel default usbcore.autosuspend (2s) wedges the RTL8812EU WFB
    # dongle, the AIC8800 management WiFi, and a USB modem the same way it
    # wedges cheap cameras. Pin power/control=on for every USB device. The
    # narrower UVC rule (50-ados-uvc-no-autosuspend) stays installed for the
    # composite-camera video4linux walk-up that this broad rule does not do.
    cat > "${usb_rule}" <<'USBEOF'
# ADOS: disable USB autosuspend on every USB device. Keeps the WFB radio,
# the management WiFi dongle, and a cellular modem from parking on the bus.
ACTION=="add", SUBSYSTEM=="usb", ATTR{power/control}="on"
USBEOF
    chmod 0644 "${usb_rule}"

    # ── 4. Ethernet EEE off ─────────────────────────────────────────────
    # Energy-Efficient-Ethernet introduces link-down/up flaps on some PHYs
    # that drops the management link when wired. Disable it on every wired
    # netdev as it appears. PHYs without EEE support fail the ethtool call
    # harmlessly (the rule ignores the result).
    local ethtool_bin
    ethtool_bin="$(_power_resolve_ethtool)"
    {
        echo '# ADOS: disable Energy-Efficient-Ethernet on wired interfaces.'
        if [ -n "${ethtool_bin}" ]; then
            echo "ACTION==\"add\", SUBSYSTEM==\"net\", KERNEL==\"eth*|end*|enP*|enx*\", RUN+=\"${ethtool_bin} --set-eee %k eee off\""
        else
            echo 'ACTION=="add", SUBSYSTEM=="net", KERNEL=="eth*|end*|enP*|enx*", RUN+="/bin/sh -c '"'"'ethtool --set-eee %k eee off'"'"'"'
        fi
    } > "${eth_rule}"
    chmod 0644 "${eth_rule}"

    # Reload udev and re-fire on already-bound devices so an upgrade applies
    # the new rules without requiring a physical replug or a reboot.
    udevadm control --reload 2>/dev/null || true
    udevadm trigger --subsystem-match=usb --action=change 2>/dev/null || true
    udevadm trigger --subsystem-match=net --action=add 2>/dev/null || true

    # ── 5. Mask system sleep ────────────────────────────────────────────
    # A drone or ground station must never suspend or hibernate. Masking
    # the sleep targets blocks every path into them (logind, systemctl
    # suspend, an idle timer). Reversible with `systemctl unmask`.
    systemctl mask sleep.target suspend.target hibernate.target \
        hybrid-sleep.target suspend-then-hibernate.target 2>/dev/null || true

    # logind drop-in: ignore the idle timer, the power key, the lid switch,
    # and the suspend key so a console keypress or a closed lid cannot put
    # the box to sleep.
    local logind_conf
    logind_conf="$(_power_path /etc/systemd/logind.conf.d/99-ados-nosleep.conf)"
    install -d -m 0755 "$(dirname "${logind_conf}")"
    cat > "${logind_conf}" <<'LOGINDEOF'
[Login]
IdleAction=ignore
HandlePowerKey=ignore
HandleLidSwitch=ignore
HandleSuspendKey=ignore
LOGINDEOF
    chmod 0644 "${logind_conf}"
    systemctl daemon-reload 2>/dev/null || true

    # ── 6. Boot re-assert oneshot ───────────────────────────────────────
    # udev RUN+= rules cover hotplug, but a device present at cold boot can
    # win the race before the rule is loaded, and a NM reload does not
    # touch a non-NM wlan. A oneshot after network-online.target sweeps
    # every wlan* power_save and every USB power/control once more.
    local bin_dir reassert_dst unit_dst
    bin_dir="$(_power_path /opt/ados/bin)"
    install -d -m 0755 "${bin_dir}"
    reassert_dst="${bin_dir}/ados-power-reassert.sh"
    local reassert_src
    if reassert_src="$(_power_resolve_data_file scripts/ados-power-reassert.sh)"; then
        install -m 0755 "${reassert_src}" "${reassert_dst}"
    else
        # Inline fallback so the oneshot still works on a tree that does not
        # ship the helper script.
        cat > "${reassert_dst}" <<'REASSERTEOF'
#!/bin/sh
# ADOS: re-assert power knobs at boot. Forgiving by design.
for _ifdir in /sys/class/net/wlan*; do
    [ -e "${_ifdir}" ] || continue
    _if="$(basename "${_ifdir}")"
    iw dev "${_if}" set power_save off 2>/dev/null || true
done
for _ctl in /sys/bus/usb/devices/*/power/control; do
    [ -w "${_ctl}" ] || continue
    echo on > "${_ctl}" 2>/dev/null || true
done
exit 0
REASSERTEOF
        chmod 0755 "${reassert_dst}"
    fi

    unit_dst="$(_power_path /etc/systemd/system/ados-power.service)"
    install -d -m 0755 "$(dirname "${unit_dst}")"
    local power_unit_src
    if power_unit_src="$(_power_resolve_data_file systemd/ados-power.service)"; then
        install -m 0644 "${power_unit_src}" "${unit_dst}"
    else
        cat > "${unit_dst}" <<'PWRUNITEOF'
[Unit]
Description=ADOS Power Hardening Re-assert
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
ExecStart=/opt/ados/bin/ados-power-reassert.sh
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
PWRUNITEOF
    fi
    chmod 0644 "${unit_dst}"
    systemctl daemon-reload 2>/dev/null || true
    systemctl enable ados-power.service 2>/dev/null || true
    # Run it now so the knobs are asserted on the current boot too.
    systemctl start ados-power.service 2>/dev/null || true

    info "Power hardening applied (WiFi/USB/EEE power-save off, sleep masked)."
}

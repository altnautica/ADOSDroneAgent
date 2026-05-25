# shellcheck shell=bash
# =============================================================================
# 07-systemd.sh — systemd unit deployment and profile-specific enable steps.
#
# install_systemd_service deploys every data/systemd/*.service file, writes
# the supervisor unit fallback, the /run/ados tmpfile rule, and the env
# file. Profile-specific enable/disable steps live in the smaller helpers
# below. The supervisor's PartOf= chain means child units stop on any
# supervisor restart — enable_ground_station_units takes care of bringing
# the ground-station children back up after install_systemd_service runs.
# =============================================================================

# ─── Install systemd Service ────────────────────────────────────────────────

install_systemd_service() {
    info "Installing systemd services (multi-process architecture)..."

    # Migrate from legacy single-service if present
    if [ -f "/etc/systemd/system/ados-agent.service" ]; then
        info "Migrating from legacy ados-agent.service..."
        systemctl stop ados-agent 2>/dev/null || true
        systemctl disable ados-agent 2>/dev/null || true
        rm -f /etc/systemd/system/ados-agent.service
    fi

    # Find systemd unit source directory
    # Check: script-level var (from upgrade clone), repo clone, script-relative
    local systemd_src=""
    if [ -n "${SYSTEMD_SRC_DIR:-}" ] && [ -d "${SYSTEMD_SRC_DIR}" ]; then
        systemd_src="${SYSTEMD_SRC_DIR}"
    elif [ -d "${INSTALL_DIR}/repo/data/systemd" ]; then
        systemd_src="${INSTALL_DIR}/repo/data/systemd"
    elif [ -d "$(dirname "$0" 2>/dev/null)/../data/systemd" ] 2>/dev/null; then
        systemd_src="$(cd "$(dirname "$0")/../data/systemd" && pwd)"
    fi

    if [ -z "$systemd_src" ] || [ ! -d "$systemd_src" ]; then
        warn "No systemd unit templates found, generating supervisor unit..."
        # Fallback: generate supervisor unit directly
        cat > "/etc/systemd/system/ados-supervisor.service" <<SVCEOF
[Unit]
Description=ADOS Drone Agent Supervisor
After=network-online.target
Wants=network-online.target

[Service]
Type=notify
ExecStart=${VENV_DIR}/bin/ados-supervisor
Restart=always
RestartSec=1
WatchdogSec=30
TimeoutStartSec=60
EnvironmentFile=-${CONFIG_DIR}/env
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
ReadWritePaths=${DATA_DIR} ${CONFIG_DIR} /run/ados
StandardOutput=journal
StandardError=journal
SyslogIdentifier=ados-supervisor

[Install]
WantedBy=multi-user.target
SVCEOF
    else
        # Deploy all unit files from data/systemd/
        local count=0
        for unit_file in "${systemd_src}"/*.service; do
            [ -f "$unit_file" ] || continue
            local unit_name
            unit_name=$(basename "$unit_file")
            # Replace venv path if different from default
            sed "s|/opt/ados/venv|${VENV_DIR}|g" "$unit_file" > "/etc/systemd/system/${unit_name}"
            count=$((count + 1))
        done
        info "Deployed ${count} systemd unit files."
    fi

    # Create /run/ados for Unix sockets (tmpfiles.d for persistence across reboots)
    mkdir -p /run/ados
    chmod 755 /run/ados
    cat > /etc/tmpfiles.d/ados.conf <<TMPEOF
d /run/ados 0755 root root -
TMPEOF

    # Plugin runtime: per-plugin Unix sockets live under
    # /run/ados/plugins. Ship the tmpfiles.d snippet from the repo when
    # available; fall back to an inline write so older trees still
    # install cleanly. systemd-tmpfiles --create runs idempotently so
    # the directory exists before the supervisor or any plugin
    # service starts.
    install_plugin_tmpfiles

    # Kernel UDP buffer ceiling for the video pipeline. Idempotent
    # drop-in at /etc/sysctl.d/99-ados-video.conf; applies on the
    # spot so the upgrade path doesn't need a reboot.
    install_video_sysctl

    # Quiet the Rockchip BSP ISP daemon on UVC-camera rigs. Self-gating;
    # a no-op on non-Rockchip boards and on boards actually running it.
    mask_unused_rockchip_isp_service

    # Write environment file
    local device_id=""
    if [ -f "${DEVICE_ID_FILE}" ]; then
        device_id=$(cat "${DEVICE_ID_FILE}")
    fi

    cat > "${CONFIG_DIR}/env" <<ENVEOF
ADOS_DEVICE_ID=${device_id}
ADOS_CONFIG=${CONFIG_DIR}/config.yaml
ADOS_RUN_DIR=/run/ados
ENVEOF

    # Provision the cgroup slice that hosts third-party plugin
    # services before the supervisor starts. The setup script runs
    # its own daemon-reload; the one below is still required for the
    # rest of the unit files we just wrote.
    install_plugin_slice

    systemctl daemon-reload

    # Enable and start supervisor (it manages all other services)
    systemctl enable ados-supervisor 2>/dev/null
    systemctl restart ados-supervisor
    info "Supervisor service enabled and started."
    info "Child services will be started by the supervisor based on hardware detection."

    # Enable the cross-profile Peripheral Manager unit and create the
    # manifest drop-in directory. Runs on both drone and ground-station
    # profiles.
    enable_universal_units

    # Tear down units that belong to the OTHER profile. Without this,
    # a rig that was previously installed under one profile and is
    # being upgraded under a different profile keeps the prior
    # profile's services running. ados-wfb (drone TX) and ados-wfb-rx
    # (GS RX) both fight for the same RTL adapter; whichever spawns
    # first claims monitor mode and the loser dies. Stale services
    # also pin radio interfaces and hold ports the new profile expects.
    disable_other_profile_units

    # Enable ground-station units if the profile demands them.
    if [ "${ADOS_PROFILE:-drone}" = "ground_station" ] || [ "${ADOS_PROFILE:-drone}" = "ground-station" ]; then
        enable_ground_station_units
    fi

    # Drop the SSH login banner so operators see the setup URL the
    # moment they log in. Idempotent.
    install_motd
}

# Disable + stop systemd units that belong to a profile OTHER than the
# one currently being installed. Idempotent: a unit that's already
# disabled is a clean no-op. Safe on a fresh install where none of these
# exist yet — every operation tolerates the not-found case.
#
# Units listed under each side are exclusive to that profile. Services
# shared between profiles (api, cloud, supervisor, mavlink, video,
# peripherals, health) are NOT touched here.
disable_other_profile_units() {
    local profile="${ADOS_PROFILE:-drone}"
    local target_units=""
    case "${profile}" in
        ground_station|ground-station)
            # On a GS rig, the drone TX manager + air-side wfb units
            # must not run. The drone uses ados-wfb (TX); GS uses
            # ados-wfb-rx (RX). Running both is the bug we are
            # cleaning up.
            target_units="ados-wfb.service"
            ;;
        *)
            # On a drone rig, every GS-only unit gets torn down. List
            # mirrors enable_ground_station_units's enable list so
            # whatever the GS install enables, the drone install
            # explicitly disables.
            target_units="ados-wfb-rx.service \
                ados-mediamtx-gs.service \
                ados-usb-gadget.service \
                ados-usb-gadget-setup.service \
                ados-oled.service \
                ados-buttons.service \
                ados-hostapd.service \
                ados-dnsmasq-gs.service \
                ados-setup-captive.service \
                ados-kiosk.service \
                ados-input.service \
                ados-pic.service \
                ados-uplink-router.service \
                ados-modem.service \
                ados-wifi-client.service \
                ados-ethernet.service \
                ados-cloud-relay.service \
                ados-batman.service \
                ados-mesh-pairing.service"
            ;;
    esac
    info "Tearing down units that do not belong to profile=${profile}..."
    for unit in ${target_units}; do
        # is-active returns nonzero for inactive/failed/missing — that's
        # fine, we still attempt disable to clear any enable-link.
        if systemctl list-unit-files "${unit}" 2>/dev/null | grep -q "${unit}"; then
            systemctl stop "${unit}" 2>/dev/null || true
            systemctl disable "${unit}" 2>/dev/null || true
        fi
    done
}

# Enable cross-profile systemd units. Run on every install regardless
# of the detected profile.
enable_universal_units() {
    info "Enabling cross-profile systemd units..."
    # ados-fbcon-detach is profile-agnostic. The unit's own
    # ConditionPathExists gates ensure it no-ops on boards that don't
    # have a provisioned SPI LCD, so enabling it everywhere is safe.
    for unit in ados-peripherals.service ados-fbcon-detach.service; do
        if [ -f "/etc/systemd/system/${unit}" ]; then
            systemctl enable "${unit}" 2>/dev/null || true
        else
            warn "Unit ${unit} not deployed; skipping enable."
        fi
    done

    # Manifest drop-in directory for /etc/ados/peripherals/*.yaml.
    mkdir -p /etc/ados/peripherals
    chmod 0755 /etc/ados/peripherals

    # Hardware-cache invalidation rules. udev fires
    # `ados hardware bust-cache` on USB add/remove for cameras, FCs,
    # USB ethernet, and v4l2 nodes so the dashboard reflects the
    # change within one polling tick instead of waiting on the
    # 30-second TTL. Always-on (no profile gate) because every
    # profile benefits.
    local hw_udev_src=""
    if [ -n "${FRESH_REPO_DIR:-}" ] && [ -f "${FRESH_REPO_DIR}/repo/data/udev/99-ados-hardware.rules" ]; then
        hw_udev_src="${FRESH_REPO_DIR}/repo/data/udev/99-ados-hardware.rules"
    elif [ -f "$(dirname "$0" 2>/dev/null)/../data/udev/99-ados-hardware.rules" ] 2>/dev/null; then
        hw_udev_src="$(cd "$(dirname "$0")/../data/udev" && pwd)/99-ados-hardware.rules"
    fi
    if [ -n "${hw_udev_src}" ] && [ -f "${hw_udev_src}" ]; then
        install -m 0644 "${hw_udev_src}" "/etc/udev/rules.d/99-ados-hardware.rules"
        udevadm control --reload 2>/dev/null || true
        info "Hardware-cache udev rules installed."
    else
        warn "Hardware-cache udev rules source not found; skipping."
    fi

    # UVC autosuspend disable rule. The kernel default usbcore.autosuspend
    # is 2 seconds — cheap USB cameras commonly mishandle resume and end
    # up wedged on the bus until a physical replug. Pin power/control=on
    # for any USB device whose bDeviceClass is 0e (video) and for the
    # parent of any /dev/video* node from a composite camera.
    local uvc_udev_src=""
    if [ -n "${FRESH_REPO_DIR:-}" ] && [ -f "${FRESH_REPO_DIR}/repo/data/udev/50-ados-uvc-no-autosuspend.rules" ]; then
        uvc_udev_src="${FRESH_REPO_DIR}/repo/data/udev/50-ados-uvc-no-autosuspend.rules"
    elif [ -f "$(dirname "$0" 2>/dev/null)/../data/udev/50-ados-uvc-no-autosuspend.rules" ] 2>/dev/null; then
        uvc_udev_src="$(cd "$(dirname "$0")/../data/udev" && pwd)/50-ados-uvc-no-autosuspend.rules"
    fi
    if [ -n "${uvc_udev_src}" ] && [ -f "${uvc_udev_src}" ]; then
        install -m 0644 "${uvc_udev_src}" "/etc/udev/rules.d/50-ados-uvc-no-autosuspend.rules"
        udevadm control --reload 2>/dev/null || true
        # Re-fire the rule on already-bound USB devices so a camera that
        # was plugged in BEFORE this install gets autosuspend disabled
        # without requiring a physical replug.
        udevadm trigger --subsystem-match=usb --action=change 2>/dev/null || true
        info "UVC autosuspend-disable udev rule installed."
    else
        warn "UVC autosuspend udev rule source not found; skipping."
    fi
}

# Stop, disable, and mask the Debian-default dnsmasq.service and
# hostapd.service units. Both get enabled the moment the apt packages
# land, and both bind ports the ADOS ground-station profile owns:
# standalone dnsmasq grabs 0.0.0.0:53 and 0.0.0.0:67, which blocks
# ados-dnsmasq-gs (wlan0:53 + wlan0:67) and ados-setup-captive
# (0.0.0.0:53). Standalone hostapd would equally fight ados-hostapd
# for wlan0 once it's configured. Mask both so they cannot be revived
# by a future apt-reinstall or by an operator reflex `systemctl start
# dnsmasq`. Called from both the fresh-install ground-station deps
# step AND the upgrade path so previously-installed rigs get the fix
# on the next `install.sh --upgrade`. Idempotent — every call is a
# no-op when already in the right state.
mask_conflicting_standalone_services() {
    systemctl stop    dnsmasq.service hostapd.service 2>/dev/null || true
    systemctl disable dnsmasq.service hostapd.service 2>/dev/null || true
    systemctl mask    dnsmasq.service hostapd.service 2>/dev/null || true
}

# The Rockchip BSP ships rkaiq_3A.service, the ISP 3A tuning daemon for
# MIPI CSI cameras. On a USB-UVC rig it has no sensor to attach to and
# lands in a failed state, cluttering `systemctl --failed` even though
# nothing ADOS uses depends on it (the video pipeline captures from
# /dev/video0 directly). Mask it ONLY when it is present and not active,
# so a board genuinely running a MIPI camera (rkaiq healthy) keeps it.
# Self-gating: a no-op on non-Rockchip boards (unit absent) and on
# boards where rkaiq is doing real work. Reversible: `systemctl unmask
# rkaiq_3A`. Runs on every install + upgrade; idempotent.
mask_unused_rockchip_isp_service() {
    systemctl list-unit-files rkaiq_3A.service >/dev/null 2>&1 || return 0
    if systemctl is-active --quiet rkaiq_3A.service; then
        return 0
    fi
    systemctl reset-failed rkaiq_3A.service 2>/dev/null || true
    systemctl mask        rkaiq_3A.service 2>/dev/null || true
}

# Enable ground-station systemd units. Safe to run on any profile; a
# no-op for drone because we branch on profile at the call site.
enable_ground_station_units() {
    info "Enabling ground-station systemd units..."

    # Install libcomposite USB gadget script + oneshot
    # composer unit. Both are gated behind ADOS_ENABLE_USB_GADGET=1
    # (default off) until founder validates on bench. The Python-side
    # ados-usb-gadget.service Manager remains in the enable list below
    # for state transitions; it no-ops when the gadget is unbound.
    if [ "${ADOS_ENABLE_USB_GADGET:-0}" = "1" ]; then
        local gadget_src=""
        if [ -n "${FRESH_REPO_DIR:-}" ] && [ -f "${FRESH_REPO_DIR}/repo/data/usb-gadget/ados-cdc-ncm-rndis.sh" ]; then
            gadget_src="${FRESH_REPO_DIR}/repo/data/usb-gadget/ados-cdc-ncm-rndis.sh"
        elif [ -f "$(dirname "$0" 2>/dev/null)/../data/usb-gadget/ados-cdc-ncm-rndis.sh" ] 2>/dev/null; then
            gadget_src="$(cd "$(dirname "$0")/../data/usb-gadget" && pwd)/ados-cdc-ncm-rndis.sh"
        fi
        if [ -n "${gadget_src}" ] && [ -f "${gadget_src}" ]; then
            install -d -m 0755 /usr/local/lib/ados/usb-gadget
            install -m 0755 "${gadget_src}" /usr/local/lib/ados/usb-gadget/ados-cdc-ncm-rndis.sh
            info "USB gadget composer script installed (ADOS_ENABLE_USB_GADGET=1)."
            # Ensure dwc2 is loaded on Pi 4B class boards so the gadget
            # subsystem has a UDC to bind to. No-op on boards that lack
            # OTG hardware.
            if ! grep -q '^dwc2' /etc/modules 2>/dev/null; then
                echo dwc2 >> /etc/modules || true
            fi
            modprobe dwc2 2>/dev/null || true
            modprobe libcomposite 2>/dev/null || true
            systemctl enable ados-usb-gadget-setup.service 2>/dev/null || true
        else
            warn "USB gadget composer script source not found; skipping (ADOS_ENABLE_USB_GADGET=1 was set)."
        fi
    fi

    for unit in \
        ados-wfb-rx.service \
        ados-mediamtx-gs.service \
        ados-usb-gadget.service \
        ados-oled.service \
        ados-buttons.service \
        ados-hostapd.service \
        ados-dnsmasq-gs.service \
        ados-setup-captive.service \
        ados-kiosk.service \
        ados-input.service \
        ados-pic.service \
        ados-uplink-router.service \
        ados-modem.service \
        ados-wifi-client.service \
        ados-ethernet.service \
        ados-cloud-relay.service; do
        if [ -f "/etc/systemd/system/${unit}" ]; then
            systemctl enable "${unit}" 2>/dev/null || true
            # Also kick the unit. enable_ground_station_units is called
            # from both the fresh-install path AND the --upgrade path,
            # and in the upgrade case the supervisor restart upstream
            # stopped these via PartOf= without anything subsequently
            # starting them. Start with --no-block so any unit whose
            # ExecStartPre takes a moment does not serialise the rest.
            # Idempotent: already-running units are a no-op for start.
            systemctl start --no-block "${unit}" 2>/dev/null || true
        else
            warn "Unit ${unit} not deployed; skipping enable."
        fi
    done

    # Ensure /etc/ados exists for AP passphrase, setup sentinel, etc.
    # /var/lib/ados, /var/log/ados, /run/ados are created upstream by
    # setup_state_dirs in 13-main.sh before any unit is deployed.
    mkdir -p /etc/ados
    chmod 0755 /etc/ados

    # Button service uses libgpiod via /dev/gpiochip0 which requires gpio group.
    # Idempotent: usermod -aG is a no-op if the user is already a member.
    if getent group gpio >/dev/null 2>&1; then
        if id ados >/dev/null 2>&1; then
            usermod -aG gpio ados || true
        fi
        if id pi >/dev/null 2>&1; then
            usermod -aG gpio pi || true
        fi
    else
        warn "gpio group not present on this system; skipping usermod -aG gpio."
    fi

    # input manager + PIC arbiter need /dev/input (gamepads, evdev) and
    # Bluetooth DBus access. Add both the `ados` service user and the
    # install-time `pi` user (if present) to the input, plugdev, and
    # bluetooth groups. All usermod calls are idempotent no-ops when
    # membership already exists. The i2c group lets the OLED and future
    # I2C peripherals be driven from userspace without root.
    for grp in input plugdev bluetooth i2c; do
        if ! getent group "${grp}" >/dev/null 2>&1; then
            warn "Group ${grp} not present on this system; skipping usermod -aG ${grp}."
            continue
        fi
        if id ados >/dev/null 2>&1; then
            usermod -aG "${grp}" ados || true
        fi
        if id pi >/dev/null 2>&1; then
            usermod -aG "${grp}" pi || true
        fi
    done

    # Trigger udev rebuild so i2c-dev nodes pick up the new group
    # membership without requiring a reboot.
    udevadm trigger --subsystem-match=i2c-dev || true

    # Install udev rules for gamepad + joystick hot-plug recognition.
    # Rule file ships in data/udev/ and is copied to /etc/udev/rules.d/.
    local udev_src=""
    if [ -n "${FRESH_REPO_DIR:-}" ] && [ -f "${FRESH_REPO_DIR}/repo/data/udev/99-ados-input.rules" ]; then
        udev_src="${FRESH_REPO_DIR}/repo/data/udev/99-ados-input.rules"
    elif [ -f "$(dirname "$0" 2>/dev/null)/../data/udev/99-ados-input.rules" ] 2>/dev/null; then
        udev_src="$(cd "$(dirname "$0")/../data/udev" && pwd)/99-ados-input.rules"
    fi
    if [ -n "${udev_src}" ] && [ -f "${udev_src}" ]; then
        install -m 0644 "${udev_src}" "/etc/udev/rules.d/99-ados-input.rules"
        info "Input udev rules installed."
    else
        warn "Input udev rules source not found; skipping 99-ados-input.rules install."
    fi

    # Install modem hot-plug udev rule when the modem stack is enabled.
    # Gated on ADOS_ENABLE_MODEM=1 (matches the modemmanager apt install gate).
    if [ "${ADOS_ENABLE_MODEM:-0}" = "1" ]; then
        local modem_udev_src=""
        if [ -n "${FRESH_REPO_DIR:-}" ] && [ -f "${FRESH_REPO_DIR}/repo/data/udev/99-ados-modem.rules" ]; then
            modem_udev_src="${FRESH_REPO_DIR}/repo/data/udev/99-ados-modem.rules"
        elif [ -f "$(dirname "$0" 2>/dev/null)/../data/udev/99-ados-modem.rules" ] 2>/dev/null; then
            modem_udev_src="$(cd "$(dirname "$0")/../data/udev" && pwd)/99-ados-modem.rules"
        fi
        if [ -n "${modem_udev_src}" ] && [ -f "${modem_udev_src}" ]; then
            install -m 0644 "${modem_udev_src}" "/etc/udev/rules.d/99-ados-modem.rules"
            info "Modem udev rules installed."
        else
            warn "Modem udev rules source not found; skipping 99-ados-modem.rules install."
        fi
    fi

    # Single reload + trigger after all rule copies (efficient).
    udevadm control --reload 2>/dev/null || true
    udevadm trigger 2>/dev/null || true

    # Static avahi service file so `_ados._tcp` is browseable even when
    # the agent process is restarting. The agent also registers the same
    # service in-process via the zeroconf library with live TXT records
    # (device_id, version); this static copy is a fallback baseline.
    local avahi_src=""
    if [ -n "${FRESH_REPO_DIR:-}" ] && [ -f "${FRESH_REPO_DIR}/repo/data/avahi/ados-gs-ap.service" ]; then
        avahi_src="${FRESH_REPO_DIR}/repo/data/avahi/ados-gs-ap.service"
    elif [ -f "$(dirname "$0" 2>/dev/null)/../data/avahi/ados-gs-ap.service" ] 2>/dev/null; then
        avahi_src="$(cd "$(dirname "$0")/../data/avahi" && pwd)/ados-gs-ap.service"
    fi
    if [ -n "${avahi_src}" ] && [ -f "${avahi_src}" ]; then
        install -d -m 0755 /etc/avahi/services
        install -m 0644 "${avahi_src}" /etc/avahi/services/ados-gs-ap.service
        info "Avahi service file installed."
        # Reload avahi so the new service file is picked up without a
        # full daemon restart (sending SIGHUP is the documented way).
        if systemctl is-active avahi-daemon >/dev/null 2>&1; then
            systemctl reload avahi-daemon 2>/dev/null \
                || systemctl restart avahi-daemon 2>/dev/null \
                || true
        fi
    else
        warn "Avahi service source not found; skipping ados-gs-ap.service install."
    fi
}

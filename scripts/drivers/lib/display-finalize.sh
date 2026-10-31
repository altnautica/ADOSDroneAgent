# Probation arming, modules-load list and runtime activation.
#
# Sourced by scripts/drivers/install-display-overlay.sh; defines functions only.
# Globals (paths, board/display ids, log colours) come from the main script.
# shellcheck shell=bash
# shellcheck disable=SC2034,SC2154

# ----------------------------------------------------------------------------
# Apply-verify-auto-revert probation.
#
# When the overlay was applied BLIND (auto mode, board declares the panel
# but it was not yet bound), a boot-critical boot-config edit is now staged
# but unconfirmed. Arm the self-heal: drop a probation marker recording the
# boot-config snapshot the apply path saved (extlinux.conf.ados-bak), and
# install a boot-time oneshot that confirms the panel on the next reboot or
# restores the snapshot if it never binds. This is the ONLY path that
# persists a boot-critical overlay without prior confirmed presence, and it
# self-heals within one reboot.
#
# Every board's apply path (Allwinner extlinux/env, Pi config.txt, Rockchip
# extlinux) snapshots the boot config it edits via snapshot_boot_config, so
# BOOT_CONFIG_SNAPSHOT + BOOT_CONFIG_PATH point at a restorable baseline. The
# probe restores that exact file if the panel never binds. The Rockchip
# u-boot-update path saves its own extlinux.conf.ados-bak; pick that up as a
# fallback when the helper did not run (BSP-managed rewrite).
arm_probation() {
    if [ "${DISPLAY_PRESENCE:-explicit}" = "spi-probation" ]; then
        install -d -m 0755 "${ETC_ADOS_DIR}"
        snapshot_path="${BOOT_CONFIG_SNAPSHOT}"
        boot_config="${BOOT_CONFIG_PATH}"
        if [ -z "${snapshot_path}" ] && [ -f "${BOOT_DIR}/extlinux/extlinux.conf.ados-bak" ]; then
            snapshot_path="${BOOT_DIR}/extlinux/extlinux.conf.ados-bak"
            boot_config="${BOOT_DIR}/extlinux/extlinux.conf"
        fi
        if [ -z "${snapshot_path}" ]; then
            # No restorable baseline means an apply path refused the boot-config
            # edit (brick-safety), so no overlay is staged: there is nothing to
            # confirm and nothing to revert. Skip arming probation rather than
            # point the boot probe at a non-existent snapshot.
            warn "No boot-config snapshot recorded; skipping probation (no blind overlay was applied)."
        else
            {
                echo "# Written by install-display-overlay.sh. A boot-critical SPI-LCD"
                echo "# overlay was applied blind (panel declared but not yet bound)."
                echo "# ados-display-probe.service confirms the panel on the next boot"
                echo "# or restores the boot config from the snapshot below."
                echo "display_id=${DISPLAY_ID}"
                echo "board=${BOARD_ID}"
                echo "snapshot=${snapshot_path}"
                echo "boot_config=${boot_config}"
                echo "expected_fb_name=$(fbtft_name_for_controller "${auto_controller:-}")"
                echo "touch_chip=${auto_touch_chip:-}"
            } > "${DISPLAY_PROBATION_FILE}"
            chmod 0644 "${DISPLAY_PROBATION_FILE}"
            info "Probation armed: ${DISPLAY_PROBATION_FILE} (snapshot ${snapshot_path})."
        fi
    fi
}

# ----------------------------------------------------------------------------
# Module load list
#
# An HDMI-touch panel loads only the resistive-touch driver (video is HDMI,
# owned by the kernel DRM driver, so no fbtft framebuffer stack). An SPI-LCD
# panel loads the fbtft driver stack plus the touch driver.
# ----------------------------------------------------------------------------
write_modules_load() {
    install -d -m 0755 "${MODULES_LOAD_DIR}"
    if [ "${DISPLAY_TYPE}" = "hdmi-touch" ]; then
        cat > "${MODULES_LOAD_FILE}" <<'EOF'
# Loaded by ados-display-overlay installer. Drives the SPI resistive-touch
# chip (ADS7846/XPT2046) exposed under /dev/input/ for an HDMI display. Video
# is HDMI (kernel DRM), so there is no fbtft framebuffer to load here.
ads7846
EOF
        info "Wrote ${MODULES_LOAD_FILE} (ads7846 only; HDMI touch)"
        modprobe ads7846 >/dev/null 2>&1 || true
    else
        cat > "${MODULES_LOAD_FILE}" <<'EOF'
# Loaded by ados-display-overlay installer. Drives the SPI LCD plus the
# resistive touch chip exposed under /dev/input/. The framebuffer lands on
# fb0 or fb1 depending on whether a DRM/HDMI driver also claims a node.
fbtft
fb_ili9486
ads7846
EOF
        info "Wrote ${MODULES_LOAD_FILE}"
        # Try to insert the modules now so the panel comes up before reboot.
        modprobe fbtft        >/dev/null 2>&1 || true
        modprobe fb_ili9486   >/dev/null 2>&1 || true
        modprobe ads7846      >/dev/null 2>&1 || true
    fi
}

# Try runtime activation before falling back to the reboot message.
# On Pi OS the `dtoverlay` tool can apply a config-tree overlay live;
# on Rockchip + Allwinner the modprobes above are usually enough once
# the device tree was edited at install time. Either way: poll
# /sys/class/graphics/fb*/name for ~5s; if a framebuffer reports the
# expected driver, the panel is bound and no reboot is needed.
activate_runtime() {
    if [ -n "${FB_NAME}" ]; then
        if command -v dtoverlay >/dev/null 2>&1 && [ -n "${DISPLAY_ID}" ]; then
            # Best-effort: dtoverlay is Pi-only and may fail on other
            # boards or when the overlay is already loaded. Suppress any
            # noise; the polling step below is the verdict.
            dtoverlay "${DISPLAY_ID}" >/dev/null 2>&1 || true
        fi

        bound_path=""
        bound_name=""
        poll_start_ts=$(date +%s)
        while [ "$(date +%s)" -lt "$((poll_start_ts + 5))" ]; do
            for fb_dev in /dev/fb0 /dev/fb1 /dev/fb2 /dev/fb3 /dev/fb4 /dev/fb5; do
                [ -e "${fb_dev}" ] || continue
                fb_name_file="/sys/class/graphics/$(basename "${fb_dev}")/name"
                [ -r "${fb_name_file}" ] || continue
                current_name=$(cat "${fb_name_file}" 2>/dev/null || true)
                if [ -n "${current_name}" ] && \
                    printf '%s' "${current_name}" | grep -q "${FB_NAME}"; then
                    bound_path="${fb_dev}"
                    bound_name="${current_name}"
                    break 2
                fi
            done
            sleep 1
        done

        if [ -n "${bound_path}" ]; then
            info "Display overlay provisioning complete. Panel bound at ${bound_path} (${bound_name})."
            exit 0
        fi
    fi
}

# Auto-mode display resolution: pick the panel and presence verdict.
#
# Sourced by scripts/drivers/install-display-overlay.sh; defines functions only.
# Globals (paths, board/display ids, log colours) come from the main script.
# shellcheck shell=bash
# shellcheck disable=SC2034,SC2154

# Resolve DISPLAY_ID=auto to a concrete panel (or "none") and set
# DISPLAY_PRESENCE / BOUND_FB plus the auto_* panel attributes. No-op when
# the operator forced a display id. Exits 0 on a board with no default panel.
resolve_auto_display() {
    if [ "${DISPLAY_ID}" = "auto" ]; then
        case "${BOARD_ID}" in
            cubie-a7z|rock-5c-lite|rock-5c|rpi4b|rpi5|pi-zero-2w)
                DISPLAY_ID="waveshare35a"
                ;;
            *)
                warn "Board ${BOARD_ID} has no auto-detect default display."
                warn "Pass --display <id> explicitly or --display none."
                exit 0
                ;;
        esac

        # Resolve the board-default panel's attributes once so the presence
        # probes know what fbtft driver + touch controller to look for.
        auto_type="$(display_type_from_yaml "${BOARD_ID}" "${DISPLAY_ID}")"
        auto_controller="$(display_key_from_yaml "${BOARD_ID}" "${DISPLAY_ID}" controller)"
        auto_touch_chip="$(display_key_from_yaml "${BOARD_ID}" "${DISPLAY_ID}" touch_chip)"

        # Auto mode is FULLY AUTOMATIC and brick-safe. It detects what is
        # physically present and resolves to it, in priority order:
        #
        #   1. SPI-LCD already bound  -> recognize it, write display.conf +
        #      marker, change NO boot config (already applied; zero risk).
        #   2. HDMI connected         -> resolve to the HDMI/kiosk path.
        #   3. I2C OLED present       -> provision the OLED (module load only).
        #   4. SPI-LCD declared but
        #      NOT bound               -> apply-verify-auto-revert: snapshot the
        #      boot config, apply the overlay, set probation. A boot-time probe
        #      confirms the panel on the next reboot or restores the snapshot,
        #      so a blind apply on a board with no panel self-heals within one
        #      reboot instead of bricking.
        #   5. nothing                -> display_id=none (disabled), no writes.
        DISPLAY_PRESENCE="none"
        BOUND_FB=""
        if [ "${auto_type}" = "spi-lcd" ] && \
            BOUND_FB="$(detect_bound_spi_panel "${auto_controller}" "${auto_touch_chip}")"; then
            DISPLAY_PRESENCE="spi-bound"
            info "SPI-LCD panel '${DISPLAY_ID}' is already bound at /dev/${BOUND_FB}; recognizing it (no boot-config change)."
        elif detect_hdmi; then
            # An HDMI display is connected. If the board declares an HDMI display
            # that carries a standalone SPI resistive-touch layer (hdmi-touch),
            # provision that touch overlay so the kiosk gets a libinput
            # touchscreen. Otherwise HDMI is video-only: the kiosk service binds
            # the DRM framebuffer directly, so there is no SPI panel to provision
            # — leave the marker and fall through to the none path.
            hdmi_touch_id="$(first_display_of_type "${BOARD_ID}" hdmi-touch)"
            if [ -n "${hdmi_touch_id}" ]; then
                DISPLAY_PRESENCE="hdmi-touch"
                DISPLAY_ID="${hdmi_touch_id}"
                info "HDMI display connected and board declares touch panel '${DISPLAY_ID}'; provisioning the SPI touch overlay."
                # DISPLAY_ID stays the hdmi-touch id; the per-board branch compiles
                # the touch-only overlay, loads ads7846, and writes the udev
                # calibration matrix. No SPI-LCD probation: there is no framebuffer
                # to confirm (video is HDMI), and the boot edit is snapshotted.
            else
                DISPLAY_PRESENCE="hdmi"
                info "HDMI display connected; auto mode resolves to the HDMI/kiosk path."
                # No HDMI touch panel modelled for this board; the kiosk path is
                # owned by the kiosk service, which binds the DRM framebuffer
                # directly. Treat HDMI as "no SPI panel to provision" and fall
                # through to the none path, but leave the marker behind so
                # display-driving services know a display surface exists.
                DISPLAY_ID="none"
            fi
        elif detect_i2c_oled; then
            DISPLAY_PRESENCE="i2c-oled"
            info "I2C OLED detected; auto mode will provision the OLED (module load only, no boot-critical overlay)."
            # The OLED needs no device-tree overlay; it binds over I2C at
            # runtime. Skip the boot-config branch by resolving to none for the
            # overlay step but still drop the marker so the OLED service runs.
            DISPLAY_ID="none"
        elif [ "${auto_type}" = "spi-lcd" ]; then
            DISPLAY_PRESENCE="spi-probation"
            info "Board ${BOARD_ID} declares SPI-LCD panel '${DISPLAY_ID}' but it is not bound yet."
            info "Applying the overlay under probation: a boot-time probe confirms the panel on the next"
            info "reboot, or auto-reverts the boot config if it never binds (self-healing, brick-safe)."
            # DISPLAY_ID stays the panel id; the per-board branch applies the
            # overlay and the probation marker is written after the snapshot.
        else
            info "No display detected (no bound SPI-LCD, no HDMI, no I2C OLED)."
            DISPLAY_ID="none"
        fi
    fi
}

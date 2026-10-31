# Per-board overlay provisioning branch.
#
# Sourced by scripts/drivers/install-display-overlay.sh; defines functions only.
# Globals (paths, board/display ids, log colours) come from the main script.
# shellcheck shell=bash
# shellcheck disable=SC2034,SC2154

# Compile / fetch and activate the overlay for BOARD_ID + DISPLAY_ID, and set
# OVERLAY_SOURCE, OVERLAY_REF and ACTIVATED_VIA for display.conf. Exits with
# the documented installer exit codes on unsupported combinations.
apply_board_overlay() {
    ACTIVATED_VIA="unknown"
    OVERLAY_SOURCE="unknown"
    OVERLAY_REF=""

    # HDMI-touch panels are provisioned by a board-family-agnostic path: compile
    # the declared touch-only overlay (an ads7846 node, no framebuffer panel) and
    # activate it with the same bootloader mechanism the SPI-LCD path uses. The
    # udev calibration matrix + ads7846 modules-load land in the shared tail. This
    # runs instead of the per-board panel case below.
    if [ "${DISPLAY_TYPE}" = "hdmi-touch" ]; then
        ovl_ref="$(display_key_from_yaml "${BOARD_ID}" "${DISPLAY_ID}" overlay_ref)"
        ovl_src="$(display_key_from_yaml "${BOARD_ID}" "${DISPLAY_ID}" overlay_source)"
        if [ -z "${ovl_ref}" ]; then
            error "Display ${DISPLAY_ID} on ${BOARD_ID} declares no overlay_ref."
            exit 4
        fi
        # Two provisioning families. The Raspberry Pi binds the touch via the
        # firmware-shipped ads7846 overlay (config.txt edit, no DTS compile). The
        # Rockchip / Allwinner boards compile the declared touch-only overlay and
        # activate it with their own bootloader mechanism.
        case "${BOARD_ID}" in
            rpi4b|rpi5|pi-zero-2w|raspberrypi)
                # No custom DTS: the Pi firmware ships the ads7846 overlay. Enable
                # SPI + append a parameterized dtoverlay=ads7846 line to config.txt.
                # provision_hdmi_touch_rpi sets ACTIVATED_VIA.
                provision_hdmi_touch_rpi
                ;;
            rock-5c-lite|rock-5c|cubie-a7z)
                info "Provisioning HDMI touch overlay '${ovl_ref}' for ${DISPLAY_ID} (source=${ovl_src:-upstream-vendored})."
                case "${ovl_src}" in
                    repo) compile_and_install_repo_dtbo "${BOARD_ID}" "${DISPLAY_ID}" ;;
                    *)    compile_and_install_upstream_dtbo "${ovl_ref}" ;;
                esac
                # Activate with the board's bootloader mechanism.
                case "${BOARD_ID}" in
                    rock-5c-lite|rock-5c)
                        if activate_overlay_rk3588 "${ovl_ref}"; then
                            if [ -f "${BOOT_DIR}/dtbo/managed.list" ] && command -v u-boot-update >/dev/null 2>&1; then
                                ACTIVATED_VIA="u-boot-update"
                            elif [ -f "${BOOT_DIR}/dtb/rockchip/overlays-list" ]; then
                                ACTIVATED_VIA="overlays-list"
                            else
                                ACTIVATED_VIA="armbianEnv"
                            fi
                        else
                            error "Could not activate HDMI touch overlay ${ovl_ref}."
                            exit 3
                        fi
                        ;;
                    cubie-a7z)
                        if activate_overlay_sun55i "${ovl_ref}"; then
                            if [ -f "${BOOT_DIR}/extlinux/extlinux.conf" ]; then
                                ACTIVATED_VIA="extlinux"
                            elif [ -f "${BOOT_DIR}/orangepiEnv.txt" ]; then
                                ACTIVATED_VIA="orangepiEnv"
                            elif [ -f "${BOOT_DIR}/armbianEnv.txt" ]; then
                                ACTIVATED_VIA="armbianEnv"
                            fi
                        else
                            error "Could not activate HDMI touch overlay ${ovl_ref}."
                            exit 3
                        fi
                        ;;
                esac
                ;;
            *)
                error "Board ${BOARD_ID} has no HDMI touch overlay activation handler."
                exit 4
                ;;
        esac
        OVERLAY_SOURCE="${ovl_src:-upstream-vendored}"
        OVERLAY_REF="${ovl_ref}"
    else

    case "${BOARD_ID}" in
        cubie-a7z)
            case "${DISPLAY_ID}" in
                waveshare35a)
                    compile_and_install_repo_dtbo "${BOARD_ID}" "${DISPLAY_ID}"
                    if activate_overlay_sun55i "${BOARD_ID}-${DISPLAY_ID}"; then
                        if [ -f "${BOOT_DIR}/extlinux/extlinux.conf" ]; then
                            ACTIVATED_VIA="extlinux"
                        elif [ -f "${BOOT_DIR}/orangepiEnv.txt" ]; then
                            ACTIVATED_VIA="orangepiEnv"
                        elif [ -f "${BOOT_DIR}/armbianEnv.txt" ]; then
                            ACTIVATED_VIA="armbianEnv"
                        fi
                    fi
                    OVERLAY_SOURCE="repo"
                    OVERLAY_REF="${BOARD_ID}-${DISPLAY_ID}.dts"
                    ;;
                *)
                    error "Display ${DISPLAY_ID} is not supported on ${BOARD_ID}."
                    exit 4
                    ;;
            esac
            ;;

        rock-5c-lite|rock-5c)
            case "${DISPLAY_ID}" in
                waveshare35a)
                    # Pick the activation-mechanism label up front so the
                    # vendored-DTBO path tags /etc/ados/display.conf
                    # consistently with the rest of the installer.
                    detect_activation_method() {
                        if [ -f "${BOOT_DIR}/dtbo/managed.list" ] && command -v u-boot-update >/dev/null 2>&1; then
                            echo "u-boot-update"
                        elif [ -f "${BOOT_DIR}/dtb/rockchip/overlays-list" ]; then
                            echo "overlays-list"
                        else
                            echo "armbianEnv"
                        fi
                    }
                    # Always compile and install the repo-vendored DTS for
                    # this combo. The BSP-shipped DTBO has wrong pendown-gpio
                    # polarity for the ADS7846 controller — the kernel IRQ
                    # never fires and no touch events reach userspace. The
                    # vendored source at data/overlays/upstream/rk3588-spi4-
                    # m2-cs0-waveshare35.dts pins pendown-gpio = ACTIVE_LOW
                    # + interrupts = EDGE_FALLING which is what the chip
                    # actually does on this carrier.
                    info "Installing vendored DTS for rock-5c-lite + waveshare35a (BSP polarity fix)."
                    compile_and_install_upstream_dtbo "rk3588-spi4-m2-cs0-waveshare35"
                    if activate_overlay_rk3588 "rk3588-spi4-m2-cs0-waveshare35"; then
                        OVERLAY_SOURCE="upstream-vendored"
                        OVERLAY_REF="rk3588-spi4-m2-cs0-waveshare35"
                        ACTIVATED_VIA="$(detect_activation_method)"
                    else
                        error "Could not activate vendored overlay."
                        exit 3
                    fi
                    ;;
                *)
                    error "Display ${DISPLAY_ID} is not supported on ${BOARD_ID}."
                    exit 4
                    ;;
            esac
            ;;

        rpi4b|rpi5|pi-zero-2w|raspberrypi)
            case "${DISPLAY_ID}" in
                waveshare35a)
                    # Pi OS Bookworm ships waveshare35a.dtbo natively in
                    # /boot/firmware/overlays/. Configuration is two lines
                    # in /boot/firmware/config.txt: dtparam=spi=on and
                    # dtoverlay=waveshare35a. The vc4-kms-v3d overlay
                    # claims fb0 + conflicts with the SPI fb so we comment
                    # it out. Reference:
                    # https://www.waveshare.com/wiki/3.5inch_RPi_LCD_(A)_Manual_Configuration
                    if [ -f "${BOOT_DIR}/firmware/config.txt" ]; then
                        PI_CONFIG="${BOOT_DIR}/firmware/config.txt"
                    elif [ -f "${BOOT_DIR}/config.txt" ]; then
                        PI_CONFIG="${BOOT_DIR}/config.txt"
                    else
                        error "Pi config.txt not found at ${BOOT_DIR}/firmware/config.txt or ${BOOT_DIR}/config.txt."
                        exit 3
                    fi
                    # Ensure waveshare35a.dtbo exists. Pi OS Bookworm doesn't
                    # ship this one (it's community-maintained); fetch from
                    # Waveshare's CDN if missing. Idempotent on re-runs.
                    PI_OVERLAYS_DIR="$(dirname "${PI_CONFIG}")/overlays"
                    if [ ! -f "${PI_OVERLAYS_DIR}/waveshare35a.dtbo" ]; then
                        info "waveshare35a.dtbo missing; fetching from Waveshare."
                        DEBIAN_FRONTEND=noninteractive apt-get -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold install -y unzip wget >/dev/null 2>&1 || true
                        WS_TMP="$(mktemp -d)"
                        if ! wget -q https://files.waveshare.com/wiki/common/Waveshare35a.zip -O "${WS_TMP}/Waveshare35a.zip"; then
                            rm -rf "${WS_TMP}"
                            error "Failed to download Waveshare35a.zip from files.waveshare.com."
                            exit 3
                        fi
                        if ! unzip -o "${WS_TMP}/Waveshare35a.zip" -d "${WS_TMP}/extracted" >/dev/null 2>&1; then
                            rm -rf "${WS_TMP}"
                            error "Failed to unzip Waveshare35a.zip."
                            exit 3
                        fi
                        DTBO_SRC="$(find "${WS_TMP}/extracted" -name "waveshare35a.dtbo" | head -1)"
                        if [ -z "${DTBO_SRC}" ] || [ ! -f "${DTBO_SRC}" ]; then
                            rm -rf "${WS_TMP}"
                            error "waveshare35a.dtbo not found inside the downloaded archive."
                            exit 3
                        fi
                        install -m 0755 "${DTBO_SRC}" "${PI_OVERLAYS_DIR}/waveshare35a.dtbo"
                        rm -rf "${WS_TMP}"
                        info "Installed ${PI_OVERLAYS_DIR}/waveshare35a.dtbo from Waveshare upstream."
                    fi
                    info "Editing ${PI_CONFIG} for Waveshare 3.5 LCD."
                    # Snapshot config.txt before editing so the boot probe can
                    # auto-revert (including re-enabling vc4-kms-v3d) if the panel
                    # never binds. Brick-safety: a touched boot config is always
                    # restorable.
                    snapshot_boot_config "${PI_CONFIG}" || exit 1
                    # Idempotently ensure dtparam=spi=on. Match a commented or
                    # uncommented variant; if missing, append.
                    if grep -qE '^[[:space:]]*#?[[:space:]]*dtparam=spi=on' "${PI_CONFIG}"; then
                        sed -i 's|^[[:space:]]*#[[:space:]]*dtparam=spi=on|dtparam=spi=on|' "${PI_CONFIG}"
                    else
                        printf '\n# Enabled by ADOS LCD overlay installer.\ndtparam=spi=on\n' >> "${PI_CONFIG}"
                    fi
                    # Idempotently ensure dtoverlay=waveshare35a.
                    if grep -qE '^[[:space:]]*#?[[:space:]]*dtoverlay=waveshare35a' "${PI_CONFIG}"; then
                        sed -i 's|^[[:space:]]*#[[:space:]]*dtoverlay=waveshare35a|dtoverlay=waveshare35a|' "${PI_CONFIG}"
                    else
                        printf 'dtoverlay=waveshare35a\n' >> "${PI_CONFIG}"
                    fi
                    # Comment out vc4-kms-v3d if active. It claims fb0 and
                    # competes with the SPI fb. Replace any uncommented
                    # dtoverlay=vc4-kms-v3d (with optional ,trailing args)
                    # with a commented version. Don't double-comment if
                    # already commented.
                    if grep -qE '^[[:space:]]*dtoverlay=vc4-kms-v3d' "${PI_CONFIG}"; then
                        sed -i 's|^\([[:space:]]*\)dtoverlay=vc4-kms-v3d|\1# dtoverlay=vc4-kms-v3d  # disabled by ADOS LCD installer (claims fb0)|' "${PI_CONFIG}"
                    fi
                    OVERLAY_SOURCE="raspberrypi"
                    OVERLAY_REF="waveshare35a"
                    ACTIVATED_VIA="config.txt"
                    info "Edits applied. Reboot to load the overlay."
                    ;;
                *)
                    error "Display ${DISPLAY_ID} is not supported on ${BOARD_ID}."
                    exit 4
                    ;;
            esac
            ;;

        *)
            warn "Board ${BOARD_ID} has no LCD overlay handler; skipping."
            exit 0
            ;;
    esac
    fi  # DISPLAY_TYPE hdmi-touch vs per-board panel case
}

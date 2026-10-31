# Writer for /etc/ados/display.conf.
#
# Sourced by scripts/drivers/install-display-overlay.sh; defines functions only.
# Globals (paths, board/display ids, log colours) come from the main script.
# shellcheck shell=bash
# shellcheck disable=SC2034,SC2154

# ----------------------------------------------------------------------------
# Display config — read by the on-board UI service + heartbeat
# ----------------------------------------------------------------------------
write_display_conf() {
    install -d -m 0755 "${ETC_ADOS_DIR}"

    # Defaults that match data/overlays/<board>-<display>.dts and the
    # vendored upstream source. Resolution / rotation come from the YAML
    # binding shape; for waveshare35a both boards land 480x320 portrait
    # (rotation 90). has_touch is true because both DTSes ship with
    # ads7846 status="okay".
    #
    # HDMI-touch panels have no framebuffer of their own (video is HDMI), so they
    # leave FB_PATH/FB_NAME empty and instead carry the libinput touch device name
    # + calibration matrix so the calibration wizard can regenerate the udev rule.
    TOUCH_DEVICE_NAME=""
    LIBINPUT_MATRIX=""
    TOUCH_X_MIN=""
    TOUCH_X_MAX=""
    TOUCH_Y_MIN=""
    TOUCH_Y_MAX=""
    if [ "${DISPLAY_TYPE}" = "hdmi-touch" ]; then
        CONTROLLER="$(display_key_from_yaml "${BOARD_ID}" "${DISPLAY_ID}" controller)"
        [ -n "${CONTROLLER}" ] || CONTROLLER="HDMI"
        TOUCH_CHIP="$(display_key_from_yaml "${BOARD_ID}" "${DISPLAY_ID}" touch_chip)"
        [ -n "${TOUCH_CHIP}" ] || TOUCH_CHIP="ADS7846"
        RESOLUTION="$(display_key_from_yaml "${BOARD_ID}" "${DISPLAY_ID}" resolution)"
        [ -n "${RESOLUTION}" ] || RESOLUTION="800x480"
        DEFAULT_ROTATION=0
        HAS_TOUCH="true"
        # No fbtft framebuffer: the DRM/HDMI driver owns the display.
        FB_PATH=""
        FB_NAME=""
        # The kernel input device name the ads7846 driver reports; the udev rule
        # and the wizard's regeneration both key on it.
        TOUCH_DEVICE_NAME="${TOUCH_CHIP} Touchscreen"
        # Read the declared touch bounds + orientation and compute the initial
        # LIBINPUT_CALIBRATION_MATRIX. The wizard refits this on the rig.
        TOUCH_X_MIN="$(display_touch_key_from_yaml "${BOARD_ID}" "${DISPLAY_ID}" x_min)"; [ -n "${TOUCH_X_MIN}" ] || TOUCH_X_MIN=0
        TOUCH_X_MAX="$(display_touch_key_from_yaml "${BOARD_ID}" "${DISPLAY_ID}" x_max)"; [ -n "${TOUCH_X_MAX}" ] || TOUCH_X_MAX=4095
        TOUCH_Y_MIN="$(display_touch_key_from_yaml "${BOARD_ID}" "${DISPLAY_ID}" y_min)"; [ -n "${TOUCH_Y_MIN}" ] || TOUCH_Y_MIN=0
        TOUCH_Y_MAX="$(display_touch_key_from_yaml "${BOARD_ID}" "${DISPLAY_ID}" y_max)"; [ -n "${TOUCH_Y_MAX}" ] || TOUCH_Y_MAX=4095
        _swap="$(display_touch_key_from_yaml "${BOARD_ID}" "${DISPLAY_ID}" swap_xy)"
        _invx="$(display_touch_key_from_yaml "${BOARD_ID}" "${DISPLAY_ID}" invert_x)"
        _invy="$(display_touch_key_from_yaml "${BOARD_ID}" "${DISPLAY_ID}" invert_y)"
        [ "${_swap}" = "true" ] && _swap=1 || _swap=0
        [ "${_invx}" = "true" ] && _invx=1 || _invx=0
        [ "${_invy}" = "true" ] && _invy=1 || _invy=0
        LIBINPUT_MATRIX="$(compute_libinput_matrix "${TOUCH_X_MIN}" "${TOUCH_X_MAX}" "${TOUCH_Y_MIN}" "${TOUCH_Y_MAX}" "${_swap}" "${_invx}" "${_invy}")"
        write_hdmi_touch_udev_rule "${TOUCH_DEVICE_NAME}" "${LIBINPUT_MATRIX}"
    else
    case "${DISPLAY_ID}" in
        waveshare35a)
            CONTROLLER="ILI9486"
            TOUCH_CHIP="ADS7846"
            RESOLUTION="480x320"
            # On Rockchip + Allwinner the SPI LCD comes up portrait-native
            # (320x480) and we rotate the canvas to landscape in PIL via
            # framebuffer.present(). On Raspberry Pi the waveshare35a.dtbo
            # rotates kernel-side so the fb is already landscape (480x320);
            # PIL must NOT rotate again or it clips the corners (verified
            # via /sys/class/graphics/fb0/virtual_size = 480,320 on rpi4b
            # at v0.18.12).
            case "${BOARD_ID}" in
                rpi4b|rpi5|pi-zero-2w|raspberrypi)
                    DEFAULT_ROTATION=0
                    ;;
                *)
                    DEFAULT_ROTATION=90
                    ;;
            esac
            HAS_TOUCH="true"
            FB_PATH="/dev/fb1"
            FB_NAME="fb_ili9486"
            ;;
        *)
            CONTROLLER="unknown"
            TOUCH_CHIP=""
            RESOLUTION=""
            DEFAULT_ROTATION=0
            HAS_TOUCH="false"
            FB_PATH="/dev/fb1"
            FB_NAME=""
            ;;
    esac
    fi

    # Preserve an operator-set rotation from a prior run. The Python
    # helper ados.services.ui.display_conf.write_rotation() and the LCD
    # Settings page both mutate this key without touching the rest of
    # the file. Earlier revisions of this installer unconditionally
    # wrote DEFAULT_ROTATION on every --upgrade, which silently reset
    # the operator's choice. The preserve logic lives in a sourceable
    # helper so tests/test_display_conf_idempotency.py exercises the
    # same code path.
    HELPERS_LIB=""
    if [ -n "${FRESH_REPO_DIR:-}" ] && [ -f "${FRESH_REPO_DIR}/repo/scripts/lib/display-conf-helpers.sh" ]; then
        HELPERS_LIB="${FRESH_REPO_DIR}/repo/scripts/lib/display-conf-helpers.sh"
    elif [ -f "$(dirname "$0" 2>/dev/null)/../lib/display-conf-helpers.sh" ] 2>/dev/null; then
        HELPERS_LIB="$(cd "$(dirname "$0")/../lib" && pwd)/display-conf-helpers.sh"
    elif [ -f /opt/ados/source/scripts/lib/display-conf-helpers.sh ]; then
        HELPERS_LIB="/opt/ados/source/scripts/lib/display-conf-helpers.sh"
    fi
    if [ -n "${HELPERS_LIB}" ]; then
        # shellcheck source=/dev/null
        . "${HELPERS_LIB}"
        PRIOR_ROTATION_HINT=""
        if [ -f "${DISPLAY_CONF}" ]; then
            PRIOR_ROTATION_HINT="$(awk -F= '/^rotation=/{print $2; exit}' "${DISPLAY_CONF}" 2>/dev/null | tr -d '[:space:]')"
        fi
        ROTATION="$(display_conf_preserve_rotation "${DISPLAY_CONF}" "${DEFAULT_ROTATION}")"
        if [ -n "${PRIOR_ROTATION_HINT}" ] && [ "${ROTATION}" = "${PRIOR_ROTATION_HINT}" ] && [ "${ROTATION}" != "${DEFAULT_ROTATION}" ]; then
            info "Preserved operator-set rotation=${ROTATION} from existing ${DISPLAY_CONF}."
        fi
    else
        warn "display-conf-helpers.sh not found; falling back to board default rotation."
        ROTATION="${DEFAULT_ROTATION}"
    fi

    # Note: /etc/ados/touch.calib is owned by the calibration wizard
    # (src/ados/services/ui/touch/transform.py) and survives every
    # installer run because we never touch that path. Keep it that way.
    cat > "${DISPLAY_CONF}" <<EOF
# Written by scripts/drivers/install-display-overlay.sh. The on-board
# UI service (ados-oled.service) and the cloud heartbeat assembler
# read this file at runtime to decide whether to attach a framebuffer
# renderer and what to advertise to Mission Control.
#
# The rotation key is operator-mutable via the Python helper
# ados.services.ui.display_conf.write_rotation() and via the LCD
# Settings page. The installer preserves an operator-set value on
# --upgrade and only falls back to the board default on a fresh
# install where the file did not exist.
display_id=${DISPLAY_ID}
board=${BOARD_ID}
type=${DISPLAY_TYPE}
controller=${CONTROLLER}
touch_chip=${TOUCH_CHIP}
has_touch=${HAS_TOUCH}
resolution=${RESOLUTION}
framebuffer_path=${FB_PATH}
framebuffer_name_expected=${FB_NAME}
rotation=${ROTATION}
overlay_source=${OVERLAY_SOURCE}
overlay_ref=${OVERLAY_REF}
activated_via=${ACTIVATED_VIA}
EOF
    # HDMI-touch panels append the libinput touch identity + calibration so the
    # on-screen wizard (POST /api/v1/display/calibrate/save) can regenerate the
    # udev matrix after a refit. Written only when those values are set.
    if [ "${DISPLAY_TYPE}" = "hdmi-touch" ]; then
        {
            echo "touch_device_name=${TOUCH_DEVICE_NAME}"
            echo "libinput_calibration_matrix=${LIBINPUT_MATRIX}"
            echo "touch_x_min=${TOUCH_X_MIN}"
            echo "touch_x_max=${TOUCH_X_MAX}"
            echo "touch_y_min=${TOUCH_Y_MIN}"
            echo "touch_y_max=${TOUCH_Y_MAX}"
            echo "udev_rule=${UDEV_TOUCH_RULE_FILE}"
        } >> "${DISPLAY_CONF}"
    fi
    chmod 0644 "${DISPLAY_CONF}"
    info "Wrote ${DISPLAY_CONF}"
}

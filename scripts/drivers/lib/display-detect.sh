# Board fingerprint and physical display presence probes.
#
# Sourced by scripts/drivers/install-display-overlay.sh; defines functions only.
# Globals (paths, board/display ids, log colours) come from the main script.
# shellcheck shell=bash
# shellcheck disable=SC2034,SC2154

# ----------------------------------------------------------------------------
# Board fingerprint helpers (mirror src/ados/hal/detect.py)
# ----------------------------------------------------------------------------
detect_board() {
    if [ -f "${ETC_ADOS_DIR}/board_override" ]; then
        tr -d '\0' < "${ETC_ADOS_DIR}/board_override" | head -n1
        return
    fi
    if [ -f /proc/device-tree/model ]; then
        local model
        model="$(tr -d '\0' < /proc/device-tree/model)"
        case "$(echo "${model}" | tr '[:upper:]' '[:lower:]')" in
            *"cubie a7z"*|*"sun55iw3p1"*) echo "cubie-a7z"; return ;;
            *"rock 5c lite"*) echo "rock-5c-lite"; return ;;
            *"rock 5c"*)      echo "rock-5c-lite"; return ;;
            *"rk3582"*)       echo "rock-5c-lite"; return ;;
            *"raspberry pi 4"*) echo "rpi4b"; return ;;
            *"raspberry pi 5"*) echo "rpi5"; return ;;
            *"raspberry pi zero 2"*) echo "pi-zero-2w"; return ;;
        esac
    fi
    if [ -f /proc/cpuinfo ]; then
        local hw
        hw="$(grep -m1 -iE '^(Hardware|Model)' /proc/cpuinfo | sed 's/.*: //')"
        case "$(echo "${hw}" | tr '[:upper:]' '[:lower:]')" in
            *"cubie a7z"*|*"a733"*) echo "cubie-a7z"; return ;;
            *"rock 5c"*|*"rk3582"*) echo "rock-5c-lite"; return ;;
            *"raspberry pi 4"*) echo "rpi4b"; return ;;
            *"raspberry pi 5"*) echo "rpi5"; return ;;
        esac
    fi
    echo ""
}


# Map an SPI-LCD controller name (ILI9486 / ST7789V / ...) to the fbtft
# driver name the kernel exports under /sys/class/graphics/fbN/name. The
# panel is "bound" only when one of those framebuffers reports this name.
fbtft_name_for_controller() {
    local controller
    controller="$(echo "$1" | tr '[:upper:]' '[:lower:]')"
    case "${controller}" in
        ili9486) echo "fb_ili9486" ;;
        ili9341) echo "fb_ili9341" ;;
        ili9340) echo "fb_ili9340" ;;
        st7789v) echo "fb_st7789v" ;;
        st7735r) echo "fb_st7735r" ;;
        hx8347d) echo "fb_hx8347d" ;;
        hx8353d) echo "fb_hx8353d" ;;
        *)       echo "" ;;
    esac
}

# ----------------------------------------------------------------------------
# Physical presence detection — brick-free, read-only sysfs probes
# ----------------------------------------------------------------------------

# Is a supported SPI-LCD panel already BOUND right now?
#
# Two independent confirmations, both required:
#   1. a framebuffer whose /sys/class/graphics/fbN/name carries the panel's
#      fbtft controller name (e.g. fb_ili9486). Matched by NAME across every
#      fb index because the SPI LCD lands on fb0 when no DRM/HDMI claims it
#      (headless), or fb1 when one does.
#   2. an input device whose /sys/class/input/eventN/device/name reports the
#      panel's resistive touch controller (e.g. "ADS7846 Touchscreen").
#
# Echoes the matched fbtft framebuffer device name (e.g. "fb0") on success
# and returns 0; returns 1 (no output) when not bound. Pure sysfs reads — no
# boot-config touch, zero brick risk.
detect_bound_spi_panel() {
    local controller="$1" touch_chip="$2"
    local fbtft_name
    fbtft_name="$(fbtft_name_for_controller "${controller}")"
    [ -n "${fbtft_name}" ] || return 1
    [ -d "${SYS_GRAPHICS_DIR}" ] || return 1

    local matched_fb=""
    local fb_dir name_file fb_name
    for fb_dir in "${SYS_GRAPHICS_DIR}"/fb*; do
        [ -d "${fb_dir}" ] || continue
        name_file="${fb_dir}/name"
        [ -r "${name_file}" ] || continue
        fb_name="$(cat "${name_file}" 2>/dev/null || true)"
        if [ -n "${fb_name}" ] && printf '%s' "${fb_name}" | grep -q "${fbtft_name}"; then
            matched_fb="$(basename "${fb_dir}")"
            break
        fi
    done
    [ -n "${matched_fb}" ] || return 1

    # Confirm the touch controller as a second, independent signal so a
    # framebuffer that merely happens to carry the fbtft name (without the
    # rest of the panel actually wired) does not falsely confirm presence.
    if [ -n "${touch_chip}" ]; then
        detect_touch_input "${touch_chip}" || return 1
    fi
    echo "${matched_fb}"
    return 0
}

# Is the panel's resistive touch controller present as an input device?
# Scans /sys/class/input/eventN/device/name for a case-insensitive match on
# the touch chip token (e.g. ADS7846). Returns 0 when found.
detect_touch_input() {
    local touch_chip
    touch_chip="$(echo "$1" | tr '[:upper:]' '[:lower:]')"
    [ -n "${touch_chip}" ] || return 1
    [ -d "${SYS_INPUT_DIR}" ] || return 1
    local ev_dir name_file dev_name
    for ev_dir in "${SYS_INPUT_DIR}"/event*; do
        [ -e "${ev_dir}/device/name" ] || continue
        name_file="${ev_dir}/device/name"
        dev_name="$(cat "${name_file}" 2>/dev/null | tr '[:upper:]' '[:lower:]' || true)"
        case "${dev_name}" in
            *"${touch_chip}"*) return 0 ;;
        esac
    done
    return 1
}

# Is an HDMI / DRM display connected? Confirmed by a DRM render node
# (/dev/dri/card0) plus at least one DRM connector reading "connected" in
# /sys/class/drm/*/status. Read-only.
detect_hdmi() {
    [ -d "${DEV_DRI_DIR}" ] || return 1
    ls "${DEV_DRI_DIR}"/card* >/dev/null 2>&1 || return 1
    [ -d "${SYS_DRM_DIR}" ] || return 1
    local status_file value
    for status_file in "${SYS_DRM_DIR}"/*/status; do
        [ -r "${status_file}" ] || continue
        value="$(cat "${status_file}" 2>/dev/null | tr '[:upper:]' '[:lower:]' || true)"
        [ "${value}" = "connected" ] && return 0
    done
    return 1
}

# Is an I2C OLED (SSD1306 / SH1106) present? An i2cdetect ACK at 0x3C or
# 0x3D on the board's I2C bus. Read-only SMBus probe; harmless to the bus.
detect_i2c_oled() {
    command -v "${I2CDETECT_BIN}" >/dev/null 2>&1 || return 1
    local out
    out="$("${I2CDETECT_BIN}" -y "${I2C_OLED_BUS}" 2>/dev/null | tr '[:upper:]' '[:lower:]' || true)"
    [ -n "${out}" ] || return 1
    # i2cdetect prints a found device as its hex address; an unprobed/empty
    # slot is "--" and a busy slot is "uu". Match the address token only.
    if printf '%s' "${out}" | grep -qE '(^| )(3c|3d)( |$)'; then
        return 0
    fi
    return 1
}

#!/usr/bin/env bash
# ADOS Ground Agent: SPI LCD overlay installer.
#
# Reads the active board's displays.supported list from its YAML
# profile, picks the first supported display (or the operator-forced
# id from --display / ADOS_DISPLAY), and provisions whatever the
# board's bootloader needs to bind the panel after a reboot:
#
#   * Allwinner sun55iw3p1 (Cubie A7Z): compiles the repo-shipped DTS
#     at data/overlays/<board>-<display>.dts via dtc, drops the DTBO
#     under /boot/overlay-user/, and appends an "fdtoverlays" /
#     "overlays" line to whichever boot config the BSP uses
#     (extlinux.conf, orangepiEnv.txt, or armbianEnv.txt).
#
#   * Rockchip RK3582 / RK3588 family (Rock 5C / 5C Lite): activates
#     the BSP-shipped DTBO at /boot/dtbo/<overlay_ref>.dtbo by
#     appending its name to overlays-list, or by running
#     update-u-boot when present. Falls back to compiling a vendored
#     copy of the source under data/overlays/upstream/ when the BSP
#     overlay package is missing (third-party Rockchip images such as
#     Armbian or Ubuntu-Rockchip).
#
# After the per-board branch, the script always:
#   * writes /etc/modules-load.d/ados-display.conf so fbtft + the
#     panel driver + ads7846 load on every boot
#   * writes /etc/ados/display.conf with the resolved display id,
#     framebuffer path, and touch state for the on-board UI service
#     and the heartbeat assembler to read at runtime
#   * tries `modprobe` so the bound panel becomes available before a
#     reboot when the kernel can hot-load the modules
#
# Idempotent. Re-running on a system that already has the overlay in
# place skips the compile and the boot-config edit; the modules-load
# and display.conf files get rewritten with the latest values.
#
# Usage:
#   sudo scripts/drivers/install-display-overlay.sh \
#       [--board <id>] [--display <id>]
#
# Both flags default to "auto", which means "consult the board YAML
# and pick the first supported display". --display none skips the
# overlay step entirely.
#
# Exit codes:
#   0  success or no-op (display id "none", board has no displays)
#   1  bad arguments or missing prerequisite
#   2  device-tree compile failure
#   3  boot-config write failure
#   4  unsupported board for the requested display

set -euo pipefail

GREEN='\033[0;32m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
NC='\033[0m'

info()  { echo -e "${GREEN}[lcd-overlay]${NC}  $*"; }
warn()  { echo -e "${YELLOW}[lcd-overlay]${NC}  $*"; }
error() { echo -e "${RED}[lcd-overlay]${NC}  $*" >&2; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
OVERLAY_DIR="${REPO_ROOT}/data/overlays"
UPSTREAM_DIR="${OVERLAY_DIR}/upstream"

# Pick the first writable temp-dir candidate. The agent runs sandboxed
# under systemd (PrivateTmp + ProtectSystem) which can render /tmp
# read-only when this script is invoked as a subprocess by the wizard.
# /run/ados is the agent's tmpfs and always writable; /var/tmp is the
# distro fallback; /tmp only works when we're called from a regular
# shell. Pick whichever lands first.
_choose_build_root() {
    local candidates=(
        "/run/ados"
        "/var/tmp"
        "${TMPDIR:-/tmp}"
        "/tmp"
    )
    for c in "${candidates[@]}"; do
        if [ -d "$c" ] && [ -w "$c" ]; then
            echo "$c"
            return 0
        fi
        # Try creating it — /run/ados may not exist on a stripped image
        # but is writable once we mkdir it.
        if mkdir -p "$c" 2>/dev/null && [ -w "$c" ]; then
            echo "$c"
            return 0
        fi
    done
    echo "/tmp"
}
BUILD_PARENT="$(_choose_build_root)"
BUILD_DIR="$(mktemp -d "${BUILD_PARENT}/ados-display-overlay.XXXXXX" 2>/dev/null \
    || mktemp -d "${BUILD_PARENT}/ados-display-overlay-$$")"
trap 'rm -rf "${BUILD_DIR}"' EXIT

DISPLAY_CONF=/etc/ados/display.conf
MODULES_LOAD_FILE=/etc/modules-load.d/ados-display.conf

# ----------------------------------------------------------------------------
# Argument parsing
# ----------------------------------------------------------------------------
BOARD_ID="${ADOS_BOARD_ID:-auto}"
DISPLAY_ID="${ADOS_DISPLAY:-auto}"

while [ $# -gt 0 ]; do
    case "$1" in
        --board)   BOARD_ID="$2"; shift 2 ;;
        --display) DISPLAY_ID="$2"; shift 2 ;;
        *) error "Unknown argument: $1"; exit 1 ;;
    esac
done

if [ "$(id -u)" -ne 0 ]; then
    error "Must run as root (sudo)."
    exit 1
fi

if [ "${DISPLAY_ID}" = "none" ]; then
    info "Display install skipped (--display none)."
    exit 0
fi

# ----------------------------------------------------------------------------
# Board fingerprint helpers (mirror src/ados/hal/detect.py)
# ----------------------------------------------------------------------------
detect_board() {
    if [ -f /etc/ados/board_override ]; then
        cat /etc/ados/board_override | tr -d '\0' | head -n1
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

if [ "${BOARD_ID}" = "auto" ]; then
    BOARD_ID="$(detect_board)"
    if [ -z "${BOARD_ID}" ]; then
        warn "Could not detect a supported board for LCD provisioning."
        warn "Set --board explicitly or pass --display none to skip."
        exit 0
    fi
    info "Detected board: ${BOARD_ID}"
fi

# ----------------------------------------------------------------------------
# Display id auto-pick from board YAML
# ----------------------------------------------------------------------------
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
    info "Auto-selected display: ${DISPLAY_ID}"
fi

# ----------------------------------------------------------------------------
# Build deps (lazy install — only when we are about to compile a DTS)
# ----------------------------------------------------------------------------
ensure_dtc() {
    if command -v dtc >/dev/null 2>&1; then
        return 0
    fi
    info "Installing device-tree-compiler..."
    DEBIAN_FRONTEND=noninteractive apt-get install -y device-tree-compiler
}

# ----------------------------------------------------------------------------
# Allwinner activation: edit the bootloader config that exists
# ----------------------------------------------------------------------------
activate_overlay_sun55i() {
    local overlay_basename="$1"   # "cubie-a7z-waveshare35a"
    local marker="ados:${overlay_basename}"

    # extlinux.conf style (most generic U-Boot extlinux setups)
    if [ -f /boot/extlinux/extlinux.conf ]; then
        if grep -q "${marker}" /boot/extlinux/extlinux.conf; then
            info "extlinux.conf already references ${overlay_basename}; skipping."
            return 0
        fi
        # Add an fdtoverlays line under each LABEL block. Anchor on a
        # known append line. If the structure is non-standard the
        # operator can fix it; this is a best-effort additive edit.
        if grep -q '^[[:space:]]*append' /boot/extlinux/extlinux.conf; then
            sed -i "/^[[:space:]]*append/a\\
    fdtoverlays /boot/overlay-user/${overlay_basename}.dtbo  # ${marker}
" /boot/extlinux/extlinux.conf
            info "Appended fdtoverlays line to extlinux.conf."
            return 0
        fi
        warn "extlinux.conf has no 'append' line; skipping automatic edit."
        return 1
    fi

    # Armbian / Orange Pi style env file
    if [ -f /boot/armbianEnv.txt ] || [ -f /boot/orangepiEnv.txt ]; then
        local env_file=/boot/armbianEnv.txt
        [ -f /boot/orangepiEnv.txt ] && env_file=/boot/orangepiEnv.txt
        if grep -q "${marker}" "${env_file}"; then
            info "${env_file} already references ${overlay_basename}; skipping."
            return 0
        fi
        if grep -q '^user_overlays=' "${env_file}"; then
            sed -i -E "s/^user_overlays=(.*)\$/user_overlays=\1 ${overlay_basename}/" "${env_file}"
        else
            echo "user_overlays=${overlay_basename}  # ${marker}" >> "${env_file}"
        fi
        info "Updated user_overlays in ${env_file}."
        return 0
    fi

    warn "No supported boot-config file found (extlinux.conf / armbianEnv.txt / orangepiEnv.txt)."
    warn "Manual activation required: load DTBO from /boot/overlay-user/${overlay_basename}.dtbo."
    return 1
}

# ----------------------------------------------------------------------------
# Rockchip activation. Three supported styles in priority order:
#   1. Radxa OS Bookworm (rsdk-b1+): managed.list + u-boot-update
#   2. Older Radxa OS / Debian: /boot/dtb/rockchip/overlays-list
#   3. Armbian / Ubuntu-Rockchip: armbianEnv.txt + update-u-boot
# ----------------------------------------------------------------------------
activate_overlay_rk3588() {
    local overlay_name="$1"   # "rk3588-spi4-m2-cs0-waveshare35"
    local dtbo_src="/boot/dtbo/${overlay_name}.dtbo"

    if [ ! -f "${dtbo_src}" ]; then
        warn "BSP DTBO not found at ${dtbo_src}; will compile from vendored source."
        return 1
    fi

    # Radxa OS Bookworm (rsdk-b1+): u-boot-update reads /boot/dtbo/
    # and regenerates /boot/extlinux/extlinux.conf with fdtoverlays
    # lines for every .dtbo (without .disabled suffix). managed.list
    # tracks which entries the system "owns" so rsetup can rebuild
    # cleanly across kernel upgrades. The disabled state is the
    # ".dtbo.disabled" filename suffix; we keep the bare .dtbo we
    # already installed.
    if [ -f /boot/dtbo/managed.list ] && command -v u-boot-update >/dev/null 2>&1; then
        local entry="${overlay_name}.dtbo"
        if grep -qx "${entry}" /boot/dtbo/managed.list; then
            info "managed.list already references ${entry}; skipping append."
        else
            echo "${entry}" >> /boot/dtbo/managed.list
            info "Appended ${entry} to /boot/dtbo/managed.list"
        fi
        # Defensive: rsetup convention is to keep INACTIVE overlays as
        # ".dtbo.disabled" and ACTIVE ones as bare ".dtbo". Our install
        # step already wrote the bare name so this is a no-op, but if
        # an older state lingered we drop the .disabled twin.
        rm -f "/boot/dtbo/${overlay_name}.dtbo.disabled"
        info "Running u-boot-update to regenerate extlinux.conf..."
        u-boot-update || {
            error "u-boot-update failed."
            return 1
        }
        # Confirm the fdtoverlays line landed.
        if grep -q "${overlay_name}" /boot/extlinux/extlinux.conf; then
            info "Overlay reference present in extlinux.conf."
        else
            warn "u-boot-update ran but extlinux.conf does not mention ${overlay_name} — check /etc/default/u-boot."
        fi
        return 0
    fi

    # Older Radxa OS / Debian Rockchip style.
    if [ -f /boot/dtb/rockchip/overlays-list ]; then
        if grep -qx "${overlay_name}" /boot/dtb/rockchip/overlays-list; then
            info "overlays-list already references ${overlay_name}; skipping."
        else
            echo "${overlay_name}" >> /boot/dtb/rockchip/overlays-list
            info "Appended ${overlay_name} to overlays-list."
        fi
        return 0
    fi

    # Armbian Rockchip style.
    if command -v update-u-boot >/dev/null 2>&1; then
        if [ -f /boot/armbianEnv.txt ]; then
            if grep -q "${overlay_name}" /boot/armbianEnv.txt; then
                info "armbianEnv.txt already references ${overlay_name}; skipping."
            else
                if grep -q '^user_overlays=' /boot/armbianEnv.txt; then
                    sed -i -E "s/^user_overlays=(.*)\$/user_overlays=\1 ${overlay_name}/" /boot/armbianEnv.txt
                else
                    echo "user_overlays=${overlay_name}" >> /boot/armbianEnv.txt
                fi
                info "Updated user_overlays in armbianEnv.txt."
            fi
            update-u-boot >/dev/null 2>&1 || warn "update-u-boot returned non-zero; check log."
            return 0
        fi
    fi

    warn "Could not locate a supported Rockchip overlay activation file."
    return 1
}

compile_and_install_repo_dtbo() {
    local board="$1"
    local display="$2"
    local src="${OVERLAY_DIR}/${board}-${display}.dts"
    if [ ! -f "${src}" ]; then
        error "Overlay source missing: ${src}"
        return 2
    fi
    ensure_dtc
    local out="${BUILD_DIR}/${board}-${display}.dtbo"
    info "Compiling ${src} -> ${out}"
    if ! dtc -@ -I dts -O dtb -o "${out}" "${src}" 2>"${BUILD_DIR}/dtc.log"; then
        error "dtc failed:"
        cat "${BUILD_DIR}/dtc.log" >&2
        return 2
    fi
    install -d -m 0755 /boot/overlay-user
    install -m 0644 "${out}" "/boot/overlay-user/${board}-${display}.dtbo"
    info "Installed /boot/overlay-user/${board}-${display}.dtbo"
}

compile_and_install_upstream_dtbo() {
    local overlay_name="$1"
    local src="${UPSTREAM_DIR}/${overlay_name}.dts"
    if [ ! -f "${src}" ]; then
        error "Vendored upstream source missing: ${src}"
        return 2
    fi
    ensure_dtc
    # The upstream DTS uses Rockchip dt-bindings macros (RK_PA3 etc.)
    # which require a cpp pass before dtc. Try to find the kernel
    # headers; fail gracefully if absent.
    local kbuild=""
    if [ -d "/lib/modules/$(uname -r)/build/include" ]; then
        kbuild="/lib/modules/$(uname -r)/build/include"
    fi
    local cpp_inc=()
    [ -n "${kbuild}" ] && cpp_inc+=("-I" "${kbuild}")
    cpp_inc+=("-I" "/usr/include")
    local pre="${BUILD_DIR}/${overlay_name}.cpp.dts"
    if ! cpp -E -x assembler-with-cpp -undef -nostdinc "${cpp_inc[@]}" \
            "${src}" -o "${pre}" 2>"${BUILD_DIR}/cpp.log"; then
        error "cpp pre-process failed (kernel headers may be missing):"
        cat "${BUILD_DIR}/cpp.log" >&2
        return 2
    fi
    local out="${BUILD_DIR}/${overlay_name}.dtbo"
    if ! dtc -@ -I dts -O dtb -o "${out}" "${pre}" 2>"${BUILD_DIR}/dtc.log"; then
        error "dtc failed:"
        cat "${BUILD_DIR}/dtc.log" >&2
        return 2
    fi
    install -d -m 0755 /boot/dtbo
    install -m 0644 "${out}" "/boot/dtbo/${overlay_name}.dtbo"
    info "Installed /boot/dtbo/${overlay_name}.dtbo"
}

# ----------------------------------------------------------------------------
# Per-board branch
# ----------------------------------------------------------------------------
ACTIVATED_VIA="unknown"
OVERLAY_SOURCE="unknown"
OVERLAY_REF=""

case "${BOARD_ID}" in
    cubie-a7z)
        case "${DISPLAY_ID}" in
            waveshare35a)
                compile_and_install_repo_dtbo "${BOARD_ID}" "${DISPLAY_ID}"
                if activate_overlay_sun55i "${BOARD_ID}-${DISPLAY_ID}"; then
                    if [ -f /boot/extlinux/extlinux.conf ]; then
                        ACTIVATED_VIA="extlinux"
                    elif [ -f /boot/orangepiEnv.txt ]; then
                        ACTIVATED_VIA="orangepiEnv"
                    elif [ -f /boot/armbianEnv.txt ]; then
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
                    if [ -f /boot/dtbo/managed.list ] && command -v u-boot-update >/dev/null 2>&1; then
                        echo "u-boot-update"
                    elif [ -f /boot/dtb/rockchip/overlays-list ]; then
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
                if [ -f /boot/firmware/config.txt ]; then
                    PI_CONFIG="/boot/firmware/config.txt"
                elif [ -f /boot/config.txt ]; then
                    PI_CONFIG="/boot/config.txt"
                else
                    error "Pi config.txt not found at /boot/firmware/config.txt or /boot/config.txt."
                    exit 3
                fi
                # Ensure waveshare35a.dtbo exists. Pi OS Bookworm doesn't
                # ship this one (it's community-maintained); fetch from
                # Waveshare's CDN if missing. Idempotent on re-runs.
                PI_OVERLAYS_DIR="$(dirname "${PI_CONFIG}")/overlays"
                if [ ! -f "${PI_OVERLAYS_DIR}/waveshare35a.dtbo" ]; then
                    info "waveshare35a.dtbo missing; fetching from Waveshare."
                    DEBIAN_FRONTEND=noninteractive apt-get install -y unzip wget >/dev/null 2>&1 || true
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

# ----------------------------------------------------------------------------
# Module load list
# ----------------------------------------------------------------------------
install -d -m 0755 /etc/modules-load.d
cat > "${MODULES_LOAD_FILE}" <<'EOF'
# Loaded by ados-display-overlay installer. Drives the SPI LCD bound
# at /dev/fb1 plus the resistive touch chip exposed under /dev/input/.
fbtft
fb_ili9486
ads7846
EOF
info "Wrote ${MODULES_LOAD_FILE}"

# Try to insert the modules now so the panel comes up before reboot.
modprobe fbtft        >/dev/null 2>&1 || true
modprobe fb_ili9486   >/dev/null 2>&1 || true
modprobe ads7846      >/dev/null 2>&1 || true

# ----------------------------------------------------------------------------
# Display config — read by the on-board UI service + heartbeat
# ----------------------------------------------------------------------------
install -d -m 0755 /etc/ados

# Defaults that match data/overlays/<board>-<display>.dts and the
# vendored upstream source. Resolution / rotation come from the YAML
# binding shape; for waveshare35a both boards land 480x320 portrait
# (rotation 90). has_touch is true because both DTSes ship with
# ads7846 status="okay".
case "${DISPLAY_ID}" in
    waveshare35a)
        CONTROLLER="ILI9486"
        TOUCH_CHIP="ADS7846"
        RESOLUTION="480x320"
        ROTATION=90
        HAS_TOUCH="true"
        FB_PATH="/dev/fb1"
        FB_NAME="fb_ili9486"
        ;;
    *)
        CONTROLLER="unknown"
        TOUCH_CHIP=""
        RESOLUTION=""
        ROTATION=0
        HAS_TOUCH="false"
        FB_PATH="/dev/fb1"
        FB_NAME=""
        ;;
esac

cat > "${DISPLAY_CONF}" <<EOF
# Written by scripts/drivers/install-display-overlay.sh. The on-board
# UI service (ados-oled.service) and the cloud heartbeat assembler
# read this file at runtime to decide whether to attach a framebuffer
# renderer and what to advertise to Mission Control.
display_id=${DISPLAY_ID}
board=${BOARD_ID}
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
chmod 0644 "${DISPLAY_CONF}"
info "Wrote ${DISPLAY_CONF}"

info "Display overlay provisioning complete. Reboot to bind the panel."
exit 0

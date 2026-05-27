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

# System paths. Overridable via env so the bats suite can point them at
# a temp tree and assert exactly which files the installer touches on
# each path. Defaults are the real on-board locations; production never
# sets these.
ETC_ADOS_DIR="${ADOS_ETC_DIR:-/etc/ados}"
MODULES_LOAD_DIR="${ADOS_MODULES_LOAD_DIR:-/etc/modules-load.d}"
BOOT_DIR="${ADOS_BOOT_DIR:-/boot}"
DISPLAY_CONF="${ADOS_DISPLAY_CONF:-${ETC_ADOS_DIR}/display.conf}"
MODULES_LOAD_FILE="${ADOS_MODULES_LOAD_FILE:-${MODULES_LOAD_DIR}/ados-display.conf}"
# Persistent marker written only when a panel is provisioned or a present
# panel is recognized; removed on the no-display path. Services that drive a
# display gate on it so they skip cleanly when no panel is attached.
DISPLAY_ENABLED_FILE="${ADOS_DISPLAY_ENABLED_FILE:-${ETC_ADOS_DIR}/display.enabled}"
# Probation marker for the apply-verify-auto-revert path. Records the
# boot-config snapshot so the boot-time probe can self-heal.
DISPLAY_PROBATION_FILE="${ADOS_DISPLAY_PROBATION_FILE:-${ETC_ADOS_DIR}/display.probation}"

# Presence-detection roots. Overridable via env so the bats suite can mock
# the kernel sysfs + /dev surfaces under a temp tree and assert the
# detection verdict without root or real hardware.
SYS_GRAPHICS_DIR="${ADOS_SYS_GRAPHICS_DIR:-/sys/class/graphics}"
SYS_INPUT_DIR="${ADOS_SYS_INPUT_DIR:-/sys/class/input}"
SYS_DRM_DIR="${ADOS_SYS_DRM_DIR:-/sys/class/drm}"
DEV_DRI_DIR="${ADOS_DEV_DRI_DIR:-/dev/dri}"
# i2cdetect can be stubbed in tests via ADOS_I2CDETECT_BIN. The I2C bus the
# board exposes the OLED on (bus 1 on every supported board today).
I2CDETECT_BIN="${ADOS_I2CDETECT_BIN:-i2cdetect}"
I2C_OLED_BUS="${ADOS_I2C_OLED_BUS:-1}"

# ----------------------------------------------------------------------------
# Argument parsing
# ----------------------------------------------------------------------------
BOARD_ID="${ADOS_BOARD_ID:-auto}"
DISPLAY_ID="${ADOS_DISPLAY:-auto}"
# Resolved presence verdict, set by the auto branch's detection ladder.
# "explicit" when the operator forced a panel id (the historical opt-in
# path: apply the overlay, no probation — the operator asserts the panel
# is wired). The auto branch overwrites this with the detected state.
DISPLAY_PRESENCE="explicit"

while [ $# -gt 0 ]; do
    case "$1" in
        --board)   BOARD_ID="$2"; shift 2 ;;
        --display) DISPLAY_ID="$2"; shift 2 ;;
        *) error "Unknown argument: $1"; exit 1 ;;
    esac
done

# Root is required to write the boot config + modules-load + display.conf
# on a real install. The bats suite redirects every write path to a temp
# tree via the ADOS_*_DIR overrides above and sets ADOS_OVERLAY_ALLOW_NONROOT
# so the auto-skip / explicit-apply branching can be exercised without root.
if [ "$(id -u)" -ne 0 ] && [ "${ADOS_OVERLAY_ALLOW_NONROOT:-0}" != "1" ]; then
    error "Must run as root (sudo)."
    exit 1
fi

if [ "${DISPLAY_ID}" = "none" ]; then
    # Explicit opt-out. Record the disabled state and remove the persistent
    # marker so display-driving services skip cleanly, matching the auto
    # no-display path. Touch nothing boot-critical.
    install -d -m 0755 "${ETC_ADOS_DIR}"
    cat > "${DISPLAY_CONF}" <<EOF
# Written by scripts/drivers/install-display-overlay.sh. Display explicitly
# disabled (--display none). Nothing written to the boot config.
display_id=none
has_touch=false
display_presence=disabled
EOF
    chmod 0644 "${DISPLAY_CONF}"
    rm -f "${DISPLAY_ENABLED_FILE}"
    info "Display install skipped (--display none); display_id=none, marker removed."
    exit 0
fi

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

# Read the `type` of a display entry from a board's HAL YAML. Scoped to
# the top-level `displays:` block (the YAML also carries an unrelated
# compute `supported:` key) and keyed on the display `id`. Pure awk so
# this works on a fresh BSP without PyYAML. Echoes the type string (e.g.
# "spi-lcd") or nothing when the board / display / type can't be found.
display_type_from_yaml() {
    local board="$1" display="$2"
    local yaml="${REPO_ROOT}/src/ados/hal/boards/${board}.yaml"
    [ -f "${yaml}" ] || return 0
    awk -v want="${display}" '
        /^displays:/ { in_displays = 1; next }
        # Any other top-level key (column 0, non-space) closes the block.
        in_displays && /^[^[:space:]]/ { in_displays = 0 }
        in_displays {
            # New list item: "    - id: <value>"
            if ($0 ~ /^[[:space:]]*-[[:space:]]*id:[[:space:]]*/) {
                line = $0
                sub(/^[[:space:]]*-[[:space:]]*id:[[:space:]]*/, "", line)
                gsub(/[[:space:]]/, "", line)
                cur_id = line
                next
            }
            if (cur_id == want && $0 ~ /^[[:space:]]*type:[[:space:]]*/) {
                line = $0
                sub(/^[[:space:]]*type:[[:space:]]*/, "", line)
                gsub(/[[:space:]]/, "", line)
                print line
                exit
            }
        }
    ' "${yaml}" 2>/dev/null
}

# ----------------------------------------------------------------------------
# Per-panel attribute readers from the board YAML
# ----------------------------------------------------------------------------

# Read an arbitrary scalar key (controller / touch_chip / ...) of a
# display entry, scoped to the top-level displays: block and keyed on the
# display id. Pure awk; mirrors display_type_from_yaml's block scoping so
# it works on a fresh BSP without PyYAML.
display_key_from_yaml() {
    local board="$1" display="$2" key="$3"
    local yaml="${REPO_ROOT}/src/ados/hal/boards/${board}.yaml"
    [ -f "${yaml}" ] || return 0
    awk -v want="${display}" -v key="${key}" '
        /^displays:/ { in_displays = 1; next }
        in_displays && /^[^[:space:]]/ { in_displays = 0 }
        in_displays {
            if ($0 ~ /^[[:space:]]*-[[:space:]]*id:[[:space:]]*/) {
                line = $0
                sub(/^[[:space:]]*-[[:space:]]*id:[[:space:]]*/, "", line)
                gsub(/[[:space:]]/, "", line)
                cur_id = line
                next
            }
            if (cur_id == want && $0 ~ ("^[[:space:]]*" key ":[[:space:]]*")) {
                line = $0
                sub(("^[[:space:]]*" key ":[[:space:]]*"), "", line)
                gsub(/[[:space:]]/, "", line)
                print line
                exit
            }
        }
    ' "${yaml}" 2>/dev/null
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
        DISPLAY_PRESENCE="hdmi"
        info "HDMI display connected; auto mode resolves to the HDMI/kiosk path."
        # No HDMI overlay panel is modelled in the board YAML today; the
        # kiosk path is owned by the kiosk service, which binds to the DRM
        # framebuffer directly. Treat HDMI as "no SPI panel to provision"
        # and fall through to the none path, but leave the marker behind so
        # display-driving services know a display surface exists.
        DISPLAY_ID="none"
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

# ----------------------------------------------------------------------------
# Already-bound SPI-LCD: recognize it, change NO boot config.
#
# The panel is physically present and the kernel has already bound it (this
# is the steady state on a Pi where the overlay was applied a boot ago, or
# any board where the panel is wired). Write display.conf describing the
# live framebuffer + the persistent marker, load the modules-load list for
# resilience across reboots, and exit. We deliberately touch nothing
# boot-critical — the overlay is already in effect, so re-applying it would
# add brick risk for zero benefit.
# ----------------------------------------------------------------------------
if [ "${DISPLAY_PRESENCE}" = "spi-bound" ]; then
    # Tag the provenance and skip the per-board overlay branch. The shared
    # tail below writes the modules-load list (so the panel re-binds across
    # reboots — modules-load.d is not boot-critical), the display.conf, and
    # the persistent marker. We deliberately make NO boot-config edit: the
    # overlay is already in effect, so re-applying it would add brick risk
    # for zero benefit.
    OVERLAY_SOURCE="present"
    OVERLAY_REF="bound:${BOUND_FB}"
    ACTIVATED_VIA="already-bound"
    SKIP_BOARD_BRANCH=1
fi

# The auto path may have resolved to "none" (no panel, or HDMI / I2C-OLED
# which need no device-tree overlay). Write a display.conf reflecting no
# SPI panel and exit cleanly so the on-board UI service and heartbeat see a
# consistent state. Nothing is written to the boot config or the
# modules-load directory on this path.
#
# Marker policy: the persistent display.enabled marker is written when a
# display SURFACE exists that a display-driving service should run for
# (HDMI for the kiosk, I2C OLED for the UI service). It is REMOVED when
# nothing is present so those services skip cleanly instead of failing.
if [ "${DISPLAY_ID}" = "none" ]; then
    install -d -m 0755 "${ETC_ADOS_DIR}"
    cat > "${DISPLAY_CONF}" <<EOF
# Written by scripts/drivers/install-display-overlay.sh. No SPI-LCD panel
# was provisioned (none requested, or a non-overlay surface was detected).
# Nothing was written to the boot config or modules-load.d.
display_id=none
board=${BOARD_ID}
has_touch=false
display_presence=${DISPLAY_PRESENCE}
EOF
    chmod 0644 "${DISPLAY_CONF}"
    case "${DISPLAY_PRESENCE}" in
        hdmi|i2c-oled)
            : > "${DISPLAY_ENABLED_FILE}"
            chmod 0644 "${DISPLAY_ENABLED_FILE}"
            info "Display surface present (${DISPLAY_PRESENCE}); wrote ${DISPLAY_ENABLED_FILE}."
            ;;
        *)
            rm -f "${DISPLAY_ENABLED_FILE}"
            info "No display present; removed ${DISPLAY_ENABLED_FILE} (services skip cleanly)."
            ;;
    esac
    info "No SPI panel provisioned; wrote ${DISPLAY_CONF} (display_id=none, presence=${DISPLAY_PRESENCE})."
    exit 0
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

# Records the path of the most recently snapshotted boot config so the
# probation marker can point the boot probe at it for auto-revert. Empty
# until snapshot_boot_config runs.
BOOT_CONFIG_SNAPSHOT=""
BOOT_CONFIG_PATH=""

# Snapshot a boot-config file before we edit it so the apply-verify-auto-
# revert probe can restore it if the panel never binds. Idempotent: keeps
# the FIRST (pristine) snapshot if one already exists, so re-runs never
# overwrite the known-good baseline with an already-edited file. Records the
# snapshot + live paths in BOOT_CONFIG_* for the probation marker. This is
# what makes a blind overlay apply brick-safe on every board: a boot config
# we touch is always restorable.
snapshot_boot_config() {
    local target="$1"
    [ -f "${target}" ] || return 0
    local bak="${target}.ados-bak"
    BOOT_CONFIG_PATH="${target}"
    if [ -f "${bak}" ]; then
        BOOT_CONFIG_SNAPSHOT="${bak}"
        return 0
    fi
    if cp -f "${target}" "${bak}" 2>/dev/null; then
        BOOT_CONFIG_SNAPSHOT="${bak}"
        info "Boot-config snapshot saved to ${bak}."
        return 0
    fi
    # Fail closed: without a restorable baseline a blind overlay edit could
    # leave the board unbootable with no auto-revert. Refuse the edit instead.
    error "Could not snapshot ${target}; refusing to edit the boot config without a restorable baseline (brick-safety)."
    return 1
}

# ----------------------------------------------------------------------------
# Allwinner activation: edit the bootloader config that exists
# ----------------------------------------------------------------------------
activate_overlay_sun55i() {
    local overlay_basename="$1"   # "cubie-a7z-waveshare35a"
    local marker="ados:${overlay_basename}"

    # extlinux.conf style (most generic U-Boot extlinux setups)
    if [ -f "${BOOT_DIR}/extlinux/extlinux.conf" ]; then
        if grep -q "${marker}" "${BOOT_DIR}/extlinux/extlinux.conf"; then
            info "extlinux.conf already references ${overlay_basename}; skipping."
            return 0
        fi
        # Snapshot before the edit so the boot probe can auto-revert if the
        # panel never binds. Brick-safety: a touched boot config is always
        # restorable.
        snapshot_boot_config "${BOOT_DIR}/extlinux/extlinux.conf" || return 1
        # Add an fdtoverlays line under each LABEL block. Anchor on a
        # known append line. If the structure is non-standard the
        # operator can fix it; this is a best-effort additive edit.
        if grep -q '^[[:space:]]*append' "${BOOT_DIR}/extlinux/extlinux.conf"; then
            sed -i "/^[[:space:]]*append/a\\
    fdtoverlays ${BOOT_DIR}/overlay-user/${overlay_basename}.dtbo  # ${marker}
" "${BOOT_DIR}/extlinux/extlinux.conf"
            info "Appended fdtoverlays line to extlinux.conf."
            return 0
        fi
        warn "extlinux.conf has no 'append' line; skipping automatic edit."
        return 1
    fi

    # Armbian / Orange Pi style env file
    if [ -f "${BOOT_DIR}/armbianEnv.txt" ] || [ -f "${BOOT_DIR}/orangepiEnv.txt" ]; then
        local env_file="${BOOT_DIR}/armbianEnv.txt"
        [ -f "${BOOT_DIR}/orangepiEnv.txt" ] && env_file="${BOOT_DIR}/orangepiEnv.txt"
        if grep -q "${marker}" "${env_file}"; then
            info "${env_file} already references ${overlay_basename}; skipping."
            return 0
        fi
        snapshot_boot_config "${env_file}" || return 1
        if grep -q '^user_overlays=' "${env_file}"; then
            sed -i -E "s/^user_overlays=(.*)\$/user_overlays=\1 ${overlay_basename}/" "${env_file}"
        else
            echo "user_overlays=${overlay_basename}  # ${marker}" >> "${env_file}"
        fi
        info "Updated user_overlays in ${env_file}."
        return 0
    fi

    warn "No supported boot-config file found (extlinux.conf / armbianEnv.txt / orangepiEnv.txt)."
    warn "Manual activation required: load DTBO from ${BOOT_DIR}/overlay-user/${overlay_basename}.dtbo."
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
    local dtbo_src="${BOOT_DIR}/dtbo/${overlay_name}.dtbo"

    if [ ! -f "${dtbo_src}" ]; then
        warn "BSP DTBO not found at ${dtbo_src}; will compile from vendored source."
        return 1
    fi

    # Radxa OS Bookworm (rsdk-b1+): u-boot-update reads the dtbo dir
    # and regenerates extlinux.conf with fdtoverlays lines for every
    # .dtbo (without .disabled suffix). managed.list tracks which
    # entries the system "owns" so rsetup can rebuild cleanly across
    # kernel upgrades. The disabled state is the ".dtbo.disabled"
    # filename suffix; we keep the bare .dtbo we already installed.
    if [ -f "${BOOT_DIR}/dtbo/managed.list" ] && command -v u-boot-update >/dev/null 2>&1; then
        local entry="${overlay_name}.dtbo"
        local managed_changed=0
        if grep -qx "${entry}" "${BOOT_DIR}/dtbo/managed.list"; then
            info "managed.list already references ${entry}; skipping append."
        else
            echo "${entry}" >> "${BOOT_DIR}/dtbo/managed.list"
            info "Appended ${entry} to ${BOOT_DIR}/dtbo/managed.list"
            managed_changed=1
        fi
        # Defensive: rsetup convention is to keep INACTIVE overlays as
        # ".dtbo.disabled" and ACTIVE ones as bare ".dtbo". Our install
        # step already wrote the bare name so this is a no-op, but if
        # an older state lingered we drop the .disabled twin.
        local disabled_path="${BOOT_DIR}/dtbo/${overlay_name}.dtbo.disabled"
        if [ -f "${disabled_path}" ]; then
            rm -f "${disabled_path}"
            managed_changed=1
        fi

        # Idempotency guard: u-boot-update on Radxa Bookworm rewrites
        # extlinux.conf, which is the bootloader's only source of truth
        # on this board. A partial / interrupted rewrite can leave the
        # file truncated, which surfaces as a bricked board on the next
        # power cycle (no kernel load, total network silence). Only
        # invoke u-boot-update when managed.list actually changed AND the
        # existing extlinux.conf doesn't already list our overlay — both
        # must be true for the regen to be worth the boot risk.
        local already_referenced=0
        if [ -f "${BOOT_DIR}/extlinux/extlinux.conf" ] && \
            grep -q "${overlay_name}" "${BOOT_DIR}/extlinux/extlinux.conf"; then
            already_referenced=1
        fi
        if [ "${managed_changed}" -eq 0 ] && [ "${already_referenced}" -eq 1 ]; then
            info "extlinux.conf already references ${overlay_name}; skipping u-boot-update."
            return 0
        fi

        # Pre-snapshot extlinux.conf for restore-on-failure. If u-boot-update
        # corrupts it (truncates, writes invalid syntax, leaves it mid-write
        # on a crash), we restore the working file so the next power cycle
        # boots cleanly. The bench-bricking incident of 2026-05-20 was the
        # trigger for adding this guard.
        local extlinux="${BOOT_DIR}/extlinux/extlinux.conf"
        local extlinux_bak="${extlinux}.ados-bak"
        local size_before=0
        if [ -f "${extlinux}" ]; then
            size_before=$(wc -c < "${extlinux}" 2>/dev/null || echo 0)
            if cp -f "${extlinux}" "${extlinux_bak}" 2>/dev/null; then
                info "extlinux.conf snapshot saved to ${extlinux_bak} (${size_before} bytes)"
                # Record for the probation marker so the boot probe restores
                # this exact baseline, not a guessed path.
                BOOT_CONFIG_SNAPSHOT="${extlinux_bak}"
                BOOT_CONFIG_PATH="${extlinux}"
            else
                # Fail closed: never run u-boot-update (which rewrites the
                # bootloader's only config) without a restorable baseline.
                error "Could not snapshot ${extlinux}; refusing to run u-boot-update without a restorable baseline (brick-safety)."
                return 1
            fi
        fi

        info "Running u-boot-update to regenerate extlinux.conf..."
        if ! u-boot-update; then
            error "u-boot-update failed; restoring extlinux.conf from snapshot."
            if [ -f "${extlinux_bak}" ]; then
                cp -f "${extlinux_bak}" "${extlinux}"
                error "Restored ${extlinux} from ${extlinux_bak}."
            fi
            return 1
        fi

        # Size sanity check: a well-formed extlinux.conf on Radxa is
        # always > 200 bytes (label, kernel path, fdt path, append). If
        # we see a suspiciously small file after the regen, restore.
        local size_after=0
        if [ -f "${extlinux}" ]; then
            size_after=$(wc -c < "${extlinux}" 2>/dev/null || echo 0)
        fi
        if [ "${size_after}" -lt 100 ]; then
            error "extlinux.conf is suspiciously small (${size_after} bytes); restoring from snapshot."
            if [ -f "${extlinux_bak}" ]; then
                cp -f "${extlinux_bak}" "${extlinux}"
                error "Restored ${extlinux} from ${extlinux_bak} (was ${size_before} bytes)."
            fi
            return 1
        fi
        info "extlinux.conf regen OK (${size_before} -> ${size_after} bytes)."

        # Confirm the fdtoverlays line landed.
        if grep -q "${overlay_name}" "${BOOT_DIR}/extlinux/extlinux.conf"; then
            info "Overlay reference present in extlinux.conf."
        else
            warn "u-boot-update ran but extlinux.conf does not mention ${overlay_name} — check /etc/default/u-boot."
        fi
        return 0
    fi

    # Older Radxa OS / Debian Rockchip style.
    if [ -f "${BOOT_DIR}/dtb/rockchip/overlays-list" ]; then
        if grep -qx "${overlay_name}" "${BOOT_DIR}/dtb/rockchip/overlays-list"; then
            info "overlays-list already references ${overlay_name}; skipping."
        else
            echo "${overlay_name}" >> "${BOOT_DIR}/dtb/rockchip/overlays-list"
            info "Appended ${overlay_name} to overlays-list."
        fi
        return 0
    fi

    # Armbian Rockchip style.
    if command -v update-u-boot >/dev/null 2>&1; then
        if [ -f "${BOOT_DIR}/armbianEnv.txt" ]; then
            if grep -q "${overlay_name}" "${BOOT_DIR}/armbianEnv.txt"; then
                info "armbianEnv.txt already references ${overlay_name}; skipping."
            else
                # Snapshot before the edit so the boot probe can auto-revert
                # if the panel never binds. Fail closed: no blind edit without
                # a restorable baseline (brick-safety).
                snapshot_boot_config "${BOOT_DIR}/armbianEnv.txt" || return 1
                if grep -q '^user_overlays=' "${BOOT_DIR}/armbianEnv.txt"; then
                    sed -i -E "s/^user_overlays=(.*)\$/user_overlays=\1 ${overlay_name}/" "${BOOT_DIR}/armbianEnv.txt"
                else
                    echo "user_overlays=${overlay_name}" >> "${BOOT_DIR}/armbianEnv.txt"
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
    install -d -m 0755 "${BOOT_DIR}/overlay-user"
    install -m 0644 "${out}" "${BOOT_DIR}/overlay-user/${board}-${display}.dtbo"
    info "Installed ${BOOT_DIR}/overlay-user/${board}-${display}.dtbo"
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
    install -d -m 0755 "${BOOT_DIR}/dtbo"
    install -m 0644 "${out}" "${BOOT_DIR}/dtbo/${overlay_name}.dtbo"
    info "Installed ${BOOT_DIR}/dtbo/${overlay_name}.dtbo"
}

# ----------------------------------------------------------------------------
# Per-board branch
#
# Skipped entirely when the panel is ALREADY BOUND (spi-bound): there is
# nothing to apply, and re-editing the boot config would only add brick
# risk. The OVERLAY_* tags were already set on that path; preserve them.
# ----------------------------------------------------------------------------
if [ "${SKIP_BOARD_BRANCH:-0}" != "1" ]; then
ACTIVATED_VIA="unknown"
OVERLAY_SOURCE="unknown"
OVERLAY_REF=""

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
fi  # SKIP_BOARD_BRANCH guard

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

# ----------------------------------------------------------------------------
# Module load list
# ----------------------------------------------------------------------------
install -d -m 0755 "${MODULES_LOAD_DIR}"
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

# ----------------------------------------------------------------------------
# Display config — read by the on-board UI service + heartbeat
# ----------------------------------------------------------------------------
install -d -m 0755 "${ETC_ADOS_DIR}"

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

# Persistent marker: a panel was provisioned or recognized. The on-board UI
# service + framebuffer-console detach gate on this file so they run for
# this board (and skip cleanly on boards with no panel). Written on every
# provisioned path (spi-bound, spi-probation, explicit). Removed on the
# none path above.
install -d -m 0755 "${ETC_ADOS_DIR}"
: > "${DISPLAY_ENABLED_FILE}"
chmod 0644 "${DISPLAY_ENABLED_FILE}"
info "Wrote ${DISPLAY_ENABLED_FILE}"

# Try runtime activation before falling back to the reboot message.
# On Pi OS the `dtoverlay` tool can apply a config-tree overlay live;
# on Rockchip + Allwinner the modprobes above are usually enough once
# the device tree was edited at install time. Either way: poll
# /sys/class/graphics/fb*/name for ~5s; if a framebuffer reports the
# expected driver, the panel is bound and no reboot is needed.
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

info "Display overlay provisioning complete. Reboot to bind the panel."
exit 0

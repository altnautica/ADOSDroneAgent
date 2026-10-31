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

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Libraries live next to this script. Fail loudly when one is missing: the
# logging helpers are themselves in a library, so this uses plain echo.
for _lib in display-log.sh display-yaml.sh display-detect.sh display-resolve.sh display-touch.sh display-dtbo.sh display-activate.sh display-board-branch.sh display-finalize.sh display-conf-write.sh; do
    [ -r "${SCRIPT_DIR}/lib/${_lib}" ] || { echo "install-display-overlay: missing ${SCRIPT_DIR}/lib/${_lib}" >&2; exit 1; }
done
unset _lib
# shellcheck source=scripts/drivers/lib/display-log.sh
. "${SCRIPT_DIR}/lib/display-log.sh"
# shellcheck source=scripts/drivers/lib/display-yaml.sh
. "${SCRIPT_DIR}/lib/display-yaml.sh"
# shellcheck source=scripts/drivers/lib/display-detect.sh
. "${SCRIPT_DIR}/lib/display-detect.sh"
# shellcheck source=scripts/drivers/lib/display-resolve.sh
. "${SCRIPT_DIR}/lib/display-resolve.sh"
# shellcheck source=scripts/drivers/lib/display-touch.sh
. "${SCRIPT_DIR}/lib/display-touch.sh"
# shellcheck source=scripts/drivers/lib/display-dtbo.sh
. "${SCRIPT_DIR}/lib/display-dtbo.sh"
# shellcheck source=scripts/drivers/lib/display-activate.sh
. "${SCRIPT_DIR}/lib/display-activate.sh"
# shellcheck source=scripts/drivers/lib/display-board-branch.sh
. "${SCRIPT_DIR}/lib/display-board-branch.sh"
# shellcheck source=scripts/drivers/lib/display-finalize.sh
. "${SCRIPT_DIR}/lib/display-finalize.sh"
# shellcheck source=scripts/drivers/lib/display-conf-write.sh
. "${SCRIPT_DIR}/lib/display-conf-write.sh"

REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
OVERLAY_DIR="${REPO_ROOT}/data/overlays"
UPSTREAM_DIR="${OVERLAY_DIR}/upstream"


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
# udev rules dir + the HDMI-touch calibration rule. Overridable so the bats
# suite can assert the LIBINPUT_CALIBRATION_MATRIX rule lands under a temp tree.
UDEV_RULES_DIR="${ADOS_UDEV_RULES_DIR:-/etc/udev/rules.d}"
UDEV_TOUCH_RULE_FILE="${ADOS_UDEV_TOUCH_RULE_FILE:-${UDEV_RULES_DIR}/99-ados-hdmi-touch.rules}"
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
# Board detection
# ----------------------------------------------------------------------------
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
# Display id auto-pick (board default + physical presence ladder)
# ----------------------------------------------------------------------------
resolve_auto_display

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

# Resolve the display type once here (a real, non-none display id is now
# fixed). The shared tail (modules-load, udev, display.conf) branches on it so
# an HDMI-touch panel loads only ads7846 and writes the libinput matrix, while
# an SPI-LCD panel keeps the fbtft driver stack + framebuffer config.
DISPLAY_TYPE="$(display_type_from_yaml "${BOARD_ID}" "${DISPLAY_ID}")"

# Records the path of the most recently snapshotted boot config so the
# probation marker can point the boot probe at it for auto-revert. Empty
# until snapshot_boot_config runs.
BOOT_CONFIG_SNAPSHOT=""
BOOT_CONFIG_PATH=""

# ----------------------------------------------------------------------------
# Per-board branch
#
# Skipped entirely when the panel is ALREADY BOUND (spi-bound): there is
# nothing to apply, and re-editing the boot config would only add brick
# risk. The OVERLAY_* tags were already set on that path; preserve them.
# ----------------------------------------------------------------------------
if [ "${SKIP_BOARD_BRANCH:-0}" != "1" ]; then
    apply_board_overlay
fi

arm_probation

write_modules_load

write_display_conf

# Persistent marker: a panel was provisioned or recognized. The on-board UI
# service + framebuffer-console detach gate on this file so they run for
# this board (and skip cleanly on boards with no panel). Written on every
# provisioned path (spi-bound, spi-probation, explicit). Removed on the
# none path above.
install -d -m 0755 "${ETC_ADOS_DIR}"
: > "${DISPLAY_ENABLED_FILE}"
chmod 0644 "${DISPLAY_ENABLED_FILE}"
info "Wrote ${DISPLAY_ENABLED_FILE}"

activate_runtime

info "Display overlay provisioning complete. Reboot to bind the panel."
exit 0

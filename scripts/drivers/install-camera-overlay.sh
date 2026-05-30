#!/usr/bin/env bash
# ADOS: universal CSI camera overlay provisioner.
#
# Reads the active board's cameras.supported list from its YAML profile and
# enables every camera whose binding sets overlay_required: true, using the
# boot mechanism the running image actually has (detected, NOT keyed on board
# id). The first concrete consumer is the Allwinner A733 (Cubie A7Z) with the
# BSP-shipped IMX214 overlay, but nothing here is board-specific: a board
# declares its camera + overlay_ref in YAML and this script brings it up.
#
# Overlay activation by detected mechanism:
#   * managed.list + u-boot-update (Radxa rsdk Bookworm/Bullseye): the BSP
#     ships the dtbo as <name>.dtbo.disabled; we enable it IN PLACE by renaming
#     to the bare <name>.dtbo, ensure it is registered in managed.list, then
#     run u-boot-update to regenerate extlinux.conf. Brick-safe: snapshot +
#     size-sanity + restore-on-failure, identical contract to the display
#     installer's Rockchip path.
#   * extlinux fdtoverlays / armbianEnv: fallbacks for non-managed images.
#
# overlay_source: bsp-disabled means "the dtbo already exists on /boot as a
# .disabled twin; enable in place" — we never compile or vendor it (Rule 30:
# no redistributing a BSP DTS we did not author).
#
# After activation the script always writes /etc/ados/camera.conf (read by the
# camera service + heartbeat assembler + the setup hardware-check), aggregates
# modules_required into /etc/modules-load.d/ados-hardware.conf, and arms a
# probation marker so a blind overlay self-heals on the next boot.
#
# A newly-staged overlay needs a reboot to take effect (u-boot reads the DT
# only at boot). The installer signals this by writing /run/ados/reboot-required
# (and /etc/ados/camera.conf state=pending_reboot); the install flow performs
# the single reboot. This keeps bring-up 100% automatic (Rule 26).
#
# Idempotent. Re-running on a board whose overlay is already enabled is a no-op
# for the boot config; camera.conf + modules-load are rewritten with current
# values.
#
# Usage:  sudo scripts/drivers/install-camera-overlay.sh [--board <id>] [--camera <id>|none]
#
# Exit codes: 0 success/no-op · 1 bad args/prereq · 3 boot-config write failure

set -euo pipefail

GREEN='\033[0;32m'; YELLOW='\033[0;33m'; RED='\033[0;31m'; NC='\033[0m'
info()  { echo -e "${GREEN}[cam-overlay]${NC}  $*"; }
warn()  { echo -e "${YELLOW}[cam-overlay]${NC}  $*"; }
error() { echo -e "${RED}[cam-overlay]${NC}  $*" >&2; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# System paths. Overridable via env so the test suite can redirect every write
# to a temp tree and assert exactly what the installer touches without root.
ETC_ADOS_DIR="${ADOS_ETC_DIR:-/etc/ados}"
MODULES_LOAD_DIR="${ADOS_MODULES_LOAD_DIR:-/etc/modules-load.d}"
BOOT_DIR="${ADOS_BOOT_DIR:-/boot}"
RUN_ADOS_DIR="${ADOS_RUN_DIR:-/run/ados}"
CAMERA_CONF="${ADOS_CAMERA_CONF:-${ETC_ADOS_DIR}/camera.conf}"
# Shared with the display installer so a board with both an LCD and a camera
# loads every module from one file.
MODULES_LOAD_FILE="${ADOS_MODULES_LOAD_FILE:-${MODULES_LOAD_DIR}/ados-hardware.conf}"
CAMERA_PROBATION_FILE="${ADOS_CAMERA_PROBATION_FILE:-${ETC_ADOS_DIR}/camera.probation}"
REBOOT_REQUIRED_FILE="${ADOS_REBOOT_REQUIRED_FILE:-${RUN_ADOS_DIR}/reboot-required}"
# Overridable so tests can stub u-boot-update and the YAML board dir.
UBOOT_UPDATE_BIN="${ADOS_UBOOT_UPDATE_BIN:-u-boot-update}"
BOARDS_DIR="${ADOS_BOARDS_DIR:-${REPO_ROOT}/src/ados/hal/boards}"

BOARD_ID="${ADOS_BOARD_ID:-auto}"
CAMERA_ID="${ADOS_CAMERA:-auto}"

while [ $# -gt 0 ]; do
    case "$1" in
        --board)  BOARD_ID="$2"; shift 2 ;;
        --camera) CAMERA_ID="$2"; shift 2 ;;
        *) error "Unknown argument: $1"; exit 1 ;;
    esac
done

if [ "$(id -u)" -ne 0 ] && [ "${ADOS_OVERLAY_ALLOW_NONROOT:-0}" != "1" ]; then
    error "Must run as root (sudo)."
    exit 1
fi

# ----------------------------------------------------------------------------
# Board fingerprint (mirrors install-display-overlay.sh detect_board)
# ----------------------------------------------------------------------------
detect_board() {
    if [ -f "${ETC_ADOS_DIR}/board_override" ]; then
        tr -d '\0' < "${ETC_ADOS_DIR}/board_override" | head -n1; return
    fi
    if [ -f /proc/device-tree/model ]; then
        local model; model="$(tr -d '\0' < /proc/device-tree/model)"
        case "$(echo "${model}" | tr '[:upper:]' '[:lower:]')" in
            *"cubie a7z"*|*"sun60iw2"*|*"sun55iw3p1"*|*"a733"*) echo "cubie-a7z"; return ;;
            *"rock 5c"*|*"rk3582"*) echo "rock-5c-lite"; return ;;
            *"raspberry pi 4"*) echo "rpi4b"; return ;;
            *"raspberry pi 5"*) echo "rpi5"; return ;;
        esac
    fi
    echo ""
}

if [ "${CAMERA_ID}" = "none" ]; then
    install -d -m 0755 "${ETC_ADOS_DIR}"
    { echo "# install-camera-overlay.sh: camera explicitly disabled (--camera none)."
      echo "camera_id=none"; echo "camera_present=false"; } > "${CAMERA_CONF}"
    chmod 0644 "${CAMERA_CONF}"
    info "Camera provisioning skipped (--camera none)."
    exit 0
fi

if [ "${BOARD_ID}" = "auto" ]; then
    BOARD_ID="$(detect_board)"
    if [ -z "${BOARD_ID}" ]; then
        info "No supported board detected for camera provisioning; nothing to do."
        exit 0
    fi
fi
info "Board: ${BOARD_ID}"

YAML="${BOARDS_DIR}/${BOARD_ID}.yaml"
if [ ! -f "${YAML}" ]; then
    info "No board YAML at ${YAML}; nothing to provision."
    exit 0
fi

# ----------------------------------------------------------------------------
# YAML readers (pure awk, scoped to the top-level cameras: block, keyed on the
# camera id — mirrors install-display-overlay.sh's display_*_from_yaml so this
# works on a fresh BSP with no PyYAML).
# ----------------------------------------------------------------------------

# Echo the id of the first camera entry whose overlay_required is true. Used by
# --camera auto. Echoes nothing when no camera requires an overlay.
first_overlay_camera_id() {
    awk '
        /^cameras:/ { in_block=1; next }
        in_block && /^[^[:space:]]/ { in_block=0 }
        in_block {
            if ($0 ~ /^[[:space:]]*-[[:space:]]*id:[[:space:]]*/) {
                line=$0; sub(/^[[:space:]]*-[[:space:]]*id:[[:space:]]*/,"",line); gsub(/[[:space:]]/,"",line)
                cur=line; req=""
            }
            if (cur != "" && $0 ~ /^[[:space:]]*overlay_required:[[:space:]]*true/) { print cur; exit }
        }
    ' "${YAML}" 2>/dev/null
}

# Echo a scalar key of the camera entry keyed on id (overlay_source, overlay_ref,
# vendor_isp, type, default_mode). Strips surrounding quotes.
camera_key_from_yaml() {
    local want="$1" key="$2"
    awk -v want="${want}" -v key="${key}" '
        /^cameras:/ { in_block=1; next }
        in_block && /^[^[:space:]]/ { in_block=0 }
        in_block {
            if ($0 ~ /^[[:space:]]*-[[:space:]]*id:[[:space:]]*/) {
                line=$0; sub(/^[[:space:]]*-[[:space:]]*id:[[:space:]]*/,"",line); gsub(/[[:space:]]/,"",line); cur=line
            }
            if (cur == want && $0 ~ ("^[[:space:]]*" key ":[[:space:]]*")) {
                line=$0; sub(("^[[:space:]]*" key ":[[:space:]]*"),"",line)
                sub(/[[:space:]]+#.*$/,"",line)   # strip any trailing inline comment
                gsub(/^[[:space:]]+|[[:space:]]+$/,"",line); gsub(/^"|"$/,"",line)
                print line; exit
            }
        }
    ' "${YAML}" 2>/dev/null
}

# Echo the modules_required list (space-separated) for the camera id. Handles
# the inline-array form `modules_required: [sunxi-vin]`.
camera_modules_from_yaml() {
    local want="$1"
    awk -v want="${want}" '
        /^cameras:/ { in_block=1; next }
        in_block && /^[^[:space:]]/ { in_block=0 }
        in_block {
            if ($0 ~ /^[[:space:]]*-[[:space:]]*id:[[:space:]]*/) {
                line=$0; sub(/^[[:space:]]*-[[:space:]]*id:[[:space:]]*/,"",line); gsub(/[[:space:]]/,"",line); cur=line
            }
            if (cur == want && $0 ~ /^[[:space:]]*modules_required:[[:space:]]*\[/) {
                line=$0; sub(/^[[:space:]]*modules_required:[[:space:]]*\[/,"",line); sub(/\].*/,"",line)
                gsub(/[[:space:]]/,"",line); gsub(/,/," ",line); print line; exit
            }
        }
    ' "${YAML}" 2>/dev/null
}

# Ensure the userspace packages a CSI/USB camera needs: the gstreamer CLI
# tools + v4l-utils. The Radxa-patched gstreamer plugins (incl. the vendor v4l2
# source) ship in the BSP; typically only these CLI tools are absent. Idempotent,
# best-effort, non-fatal.
ensure_camera_packages() {
    command -v apt-get >/dev/null 2>&1 || return 0
    local need=(gstreamer1.0-tools v4l-utils) missing=() p
    for p in "${need[@]}"; do
        dpkg -s "${p}" >/dev/null 2>&1 || missing+=("${p}")
    done
    if [ "${#missing[@]}" -eq 0 ]; then
        info "Camera userspace packages already present."
        return 0
    fi
    info "Installing camera userspace packages: ${missing[*]}"
    DEBIAN_FRONTEND=noninteractive apt-get install -y "${missing[@]}" >/dev/null 2>&1 \
        || warn "Could not install ${missing[*]} (offline?); camera capture tools may be unavailable."
}

if [ "${CAMERA_ID}" = "auto" ]; then
    CAMERA_ID="$(first_overlay_camera_id)"
    if [ -z "${CAMERA_ID}" ]; then
        info "Board ${BOARD_ID} declares no overlay-required camera; nothing to provision."
        # Still record a clean state so the heartbeat/hardware-check is consistent.
        install -d -m 0755 "${ETC_ADOS_DIR}"
        { echo "# install-camera-overlay.sh: no overlay-required camera declared."
          echo "camera_id=none"; echo "board=${BOARD_ID}"; echo "camera_present=false"; } > "${CAMERA_CONF}"
        chmod 0644 "${CAMERA_CONF}"
        exit 0
    fi
fi
info "Camera: ${CAMERA_ID}"
ensure_camera_packages

OVERLAY_SOURCE="$(camera_key_from_yaml "${CAMERA_ID}" overlay_source)"
OVERLAY_REF="$(camera_key_from_yaml "${CAMERA_ID}" overlay_ref)"
VENDOR_ISP="$(camera_key_from_yaml "${CAMERA_ID}" vendor_isp)"
DEFAULT_MODE="$(camera_key_from_yaml "${CAMERA_ID}" default_mode)"
SENSOR="$(camera_key_from_yaml "${CAMERA_ID}" sensor)"
MODULES="$(camera_modules_from_yaml "${CAMERA_ID}")"

# ----------------------------------------------------------------------------
# Boot mechanism detection (NOT board-id keyed). Returns the activation path
# the running image actually uses.
# ----------------------------------------------------------------------------
detect_boot_mechanism() {
    if [ -f "${BOOT_DIR}/dtbo/managed.list" ] && command -v "${UBOOT_UPDATE_BIN}" >/dev/null 2>&1; then
        echo "managed-list"; return
    fi
    if [ -f "${BOOT_DIR}/armbianEnv.txt" ] || [ -f "${BOOT_DIR}/orangepiEnv.txt" ]; then
        echo "armbian"; return
    fi
    if [ -f "${BOOT_DIR}/extlinux/extlinux.conf" ]; then echo "extlinux"; return; fi
    echo "none"
}

# Enable a BSP-shipped .dtbo.disabled overlay in place on the managed.list
# image, brick-safely. Globs for the overlay by overlay_ref token so the BSP
# board-family prefix (e.g. cubie-a7a-) does not need to be hardcoded. Echoes
# the enabled overlay basename (no extension) on success; returns 1 on no-op
# (already enabled) signalled via NEWLY_ENABLED=0.
NEWLY_ENABLED=0
enable_managed_list_bsp_disabled() {
    local ref="$1"
    local dtbo_dir="${BOOT_DIR}/dtbo"
    [ -d "${dtbo_dir}" ] || { error "No ${dtbo_dir}; cannot enable BSP overlay."; return 1; }

    # Prefer an already-enabled bare .dtbo; else find the .disabled twin.
    # Glob (not ls|grep) so non-alphanumeric filenames are safe; the *.dtbo
    # glob never matches *.dtbo.disabled (different suffix).
    local bare="" disabled="" name f
    shopt -s nullglob
    for f in "${dtbo_dir}"/*"${ref}"*.dtbo;          do bare="$f"; break; done
    for f in "${dtbo_dir}"/*"${ref}"*.dtbo.disabled; do disabled="$f"; break; done
    shopt -u nullglob

    if [ -n "${bare}" ]; then
        name="$(basename "${bare}" .dtbo)"
        info "Overlay ${name}.dtbo already enabled."
    elif [ -n "${disabled}" ]; then
        name="$(basename "${disabled}" .dtbo.disabled)"
        info "Enabling BSP overlay in place: ${name}.dtbo.disabled -> ${name}.dtbo"
        mv -f "${disabled}" "${dtbo_dir}/${name}.dtbo"
        NEWLY_ENABLED=1
    else
        error "No overlay matching '*${ref}*' under ${dtbo_dir} (.dtbo or .dtbo.disabled)."
        return 1
    fi

    # Ensure managed.list registers it (BSP overlays usually already are).
    local entry="${name}.dtbo"
    if ! grep -qx "${entry}" "${dtbo_dir}/managed.list" 2>/dev/null; then
        echo "${entry}" >> "${dtbo_dir}/managed.list"
        info "Registered ${entry} in managed.list."
        NEWLY_ENABLED=1
    fi

    # If nothing changed and extlinux already references it, no reboot/regen needed.
    local extlinux="${BOOT_DIR}/extlinux/extlinux.conf"
    if [ "${NEWLY_ENABLED}" -eq 0 ] && [ -f "${extlinux}" ] && grep -q "${name}" "${extlinux}"; then
        info "extlinux.conf already references ${name}; no u-boot-update needed."
        ENABLED_OVERLAY="${name}"
        return 0
    fi

    # Brick-safe u-boot-update: snapshot, regen, size-sanity, restore-on-fail.
    if [ -f "${extlinux}" ]; then
        local bak="${extlinux}.ados-bak" size_before size_after
        size_before=$(wc -c < "${extlinux}" 2>/dev/null || echo 0)
        if ! cp -f "${extlinux}" "${bak}" 2>/dev/null; then
            error "Could not snapshot ${extlinux}; refusing u-boot-update (brick-safety)."
            return 1
        fi
        BOOT_CONFIG_SNAPSHOT="${bak}"; BOOT_CONFIG_PATH="${extlinux}"
        info "Snapshot saved: ${bak} (${size_before} bytes)."
        info "Running ${UBOOT_UPDATE_BIN} to regenerate extlinux.conf..."
        if ! "${UBOOT_UPDATE_BIN}"; then
            error "u-boot-update failed; restoring extlinux.conf."
            cp -f "${bak}" "${extlinux}"; return 1
        fi
        size_after=$(wc -c < "${extlinux}" 2>/dev/null || echo 0)
        if [ "${size_after}" -lt 100 ]; then
            error "extlinux.conf suspiciously small (${size_after}b); restoring."
            cp -f "${bak}" "${extlinux}"; return 1
        fi
        info "extlinux.conf regen OK (${size_before} -> ${size_after} bytes)."
    else
        warn "No extlinux.conf; ran rename + managed.list only."
    fi
    ENABLED_OVERLAY="${name}"
    return 0
}

# ----------------------------------------------------------------------------
# Activate
# ----------------------------------------------------------------------------
BOOT_CONFIG_SNAPSHOT=""; BOOT_CONFIG_PATH=""; ENABLED_OVERLAY=""
ACTIVATED_VIA="none"; OVERLAY_STATE="staged"

if [ -z "${OVERLAY_REF}" ]; then
    warn "Camera ${CAMERA_ID} has no overlay_ref; recording present-without-overlay."
    OVERLAY_STATE="no-overlay"
else
    MECH="$(detect_boot_mechanism)"
    info "Boot mechanism: ${MECH}; overlay_source: ${OVERLAY_SOURCE:-unset}"
    case "${MECH}:${OVERLAY_SOURCE}" in
        managed-list:bsp-disabled|managed-list:)
            if enable_managed_list_bsp_disabled "${OVERLAY_REF}"; then
                ACTIVATED_VIA="u-boot-update"
                if [ "${NEWLY_ENABLED}" -eq 1 ]; then OVERLAY_STATE="pending_reboot"; else OVERLAY_STATE="enabled"; fi
            else
                error "Failed to enable overlay ${OVERLAY_REF} via managed.list."
                OVERLAY_STATE="failed"
            fi
            ;;
        *)
            warn "Boot mechanism '${MECH}' + overlay_source '${OVERLAY_SOURCE}' not yet handled for cameras; recording staged."
            OVERLAY_STATE="staged"
            ;;
    esac
fi

# ----------------------------------------------------------------------------
# Module-load aggregation (shared file; merge, do not clobber the display's)
# ----------------------------------------------------------------------------
if [ -n "${MODULES}" ]; then
    install -d -m 0755 "${MODULES_LOAD_DIR}"
    touch "${MODULES_LOAD_FILE}"
    for m in ${MODULES}; do
        grep -qxF "${m}" "${MODULES_LOAD_FILE}" 2>/dev/null || echo "${m}" >> "${MODULES_LOAD_FILE}"
        modprobe "${m}" >/dev/null 2>&1 || true
    done
    info "Ensured modules in ${MODULES_LOAD_FILE}: ${MODULES}"
fi

# ----------------------------------------------------------------------------
# Probation: a blind overlay self-heals on the next boot.
# ----------------------------------------------------------------------------
if [ "${OVERLAY_STATE}" = "pending_reboot" ] && [ -n "${BOOT_CONFIG_SNAPSHOT}" ]; then
    install -d -m 0755 "${ETC_ADOS_DIR}"
    { echo "# install-camera-overlay.sh probation: confirm CSI camera on next boot or restore."
      echo "camera_id=${CAMERA_ID}"; echo "board=${BOARD_ID}"
      echo "overlay=${ENABLED_OVERLAY}"; echo "snapshot=${BOOT_CONFIG_SNAPSHOT}"
      echo "boot_config=${BOOT_CONFIG_PATH}"; } > "${CAMERA_PROBATION_FILE}"
    chmod 0644 "${CAMERA_PROBATION_FILE}"
    info "Probation armed: ${CAMERA_PROBATION_FILE}."
fi

# ----------------------------------------------------------------------------
# camera.conf — read by the camera service, heartbeat assembler, hardware-check
# ----------------------------------------------------------------------------
install -d -m 0755 "${ETC_ADOS_DIR}"
cat > "${CAMERA_CONF}" <<EOF
# Written by scripts/drivers/install-camera-overlay.sh. The camera service and
# the cloud heartbeat assembler read this at runtime to decide whether a CSI
# camera is provisioned and how to ingest it.
camera_id=${CAMERA_ID}
board=${BOARD_ID}
sensor=${SENSOR}
camera_present=true
overlay_ref=${ENABLED_OVERLAY:-${OVERLAY_REF}}
overlay_source=${OVERLAY_SOURCE}
overlay_state=${OVERLAY_STATE}
vendor_isp=${VENDOR_ISP:-false}
default_mode=${DEFAULT_MODE}
activated_via=${ACTIVATED_VIA}
EOF
chmod 0644 "${CAMERA_CONF}"
info "Wrote ${CAMERA_CONF} (overlay_state=${OVERLAY_STATE})."

# Signal the install flow to perform the single automatic reboot (Rule 26).
if [ "${OVERLAY_STATE}" = "pending_reboot" ]; then
    install -d -m 0755 "${RUN_ADOS_DIR}" 2>/dev/null || true
    echo "camera-overlay ${ENABLED_OVERLAY}" >> "${REBOOT_REQUIRED_FILE}" 2>/dev/null || true
    info "Camera overlay staged; reboot required to bind (signalled in ${REBOOT_REQUIRED_FILE})."
fi
exit 0

#!/usr/bin/env bash
# ADOS Ground Agent: RTL8812EU DKMS driver installer.
#
# Builds and installs the vendored RTL8812EU driver via DKMS so it
# survives kernel upgrades. Idempotent: re-running is a no-op when the
# module is already built and loaded.
#
# Vendored source lives at vendor/rtl8812eu/ (git submodule).
# The mesh-enable patch at data/driver-patches/mesh-enable.patch is
# applied to the Makefile before DKMS registers the source, so 802.11s
# mesh point mode is compiled into the final kernel module.
#
# Usage:
#   sudo scripts/drivers/install-rtl8812eu.sh
#
# Exit codes:
#   0  success (module installed or already present)
#   1  missing dependency (dkms, headers, submodule)
#   2  dkms build or install failure
#   3  modprobe or verification failure

set -euo pipefail

GREEN='\033[0;32m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
NC='\033[0m'

info()  { echo -e "${GREEN}[rtl8812eu]${NC}  $*"; }
warn()  { echo -e "${YELLOW}[rtl8812eu]${NC}  $*"; }
error() { echo -e "${RED}[rtl8812eu]${NC}  $*" >&2; }

# Resolve repo root (script is at scripts/drivers/install-rtl8812eu.sh)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
VENDOR_DIR="${REPO_ROOT}/vendor/rtl8812eu"
PATCH_FILE="${REPO_ROOT}/data/driver-patches/mesh-enable.patch"

# Module name exposed by the 8822E driver build
MODULE_NAME="8812eu"

if [ "$(id -u)" -ne 0 ]; then
    error "Must run as root (sudo)."
    exit 1
fi

KERNEL="$(uname -r)"
info "Kernel: ${KERNEL}"

# Translate the running machine arch to the kernel's ARCH naming. The
# vendored Makefile resolves ARCH from `uname -m`, which on aarch64 hosts
# yields the literal "aarch64" that the kernel build rejects (it expects
# "arm64"). Same for armv7 (kernel uses "arm").
case "$(uname -m)" in
    aarch64)       export ARCH=arm64 ;;
    armv6l|armv7l) export ARCH=arm ;;
esac

# Resolve the DKMS package + version up front (best-effort) so the fast-path
# checks the EXACT installed source tree rather than globbing /usr/src, which
# would pick the wrong tree on a box carrying more than one build version.
_dkms_pkg=""; _dkms_ver=""
if [ -f "${VENDOR_DIR}/dkms.conf" ]; then
    _dkms_pkg="$(awk -F'"' '/^PACKAGE_NAME=/ {print $2}' "${VENDOR_DIR}/dkms.conf" | head -n1)"
    _dkms_ver="$(awk -F'"' '/^PACKAGE_VERSION=/ {print $2}' "${VENDOR_DIR}/dkms.conf" | head -n1)"
fi

# Fast-path: short-circuit ONLY when our DKMS build is installed on disk
# (loaded + resolvable via modinfo + registered with DKMS) AND its DKMS
# source carries the current source patches. The patch marker below is the
# tell: a build from before a driver-patch change reports as installed but
# lacks the fix, so it must be rebuilt. Update the marker string whenever a
# new source patch lands. A module merely resident in RAM (e.g. left over
# after a `dkms remove`), or an unpatched stock/BSP module, is not trusted:
# we fall through and (re)build it via DKMS below.
PATCH_MARKER="MLME_IS_MONITOR(padapter) || MLME_IS_NULL(padapter)"
if lsmod | awk '{print $1}' | grep -qx "${MODULE_NAME}"; then
    if [ -n "${_dkms_pkg}" ] && [ -n "${_dkms_ver}" ]; then
        _dkms_src="/usr/src/${_dkms_pkg}-${_dkms_ver}/core/rtw_mlme_ext.c"
    else
        _dkms_src="$(ls -d /usr/src/realtek-rtl88x2eu-*/core/rtw_mlme_ext.c 2>/dev/null | head -n1)"
    fi
    if modinfo "${MODULE_NAME}" >/dev/null 2>&1 \
       && dkms status 2>/dev/null | grep -qiE 'rtl88x2eu|8812' \
       && [ -f "${_dkms_src}" ] \
       && grep -qF "${PATCH_MARKER}" "${_dkms_src}"; then
        info "${MODULE_NAME} already installed via DKMS (current patches) and loaded."
        # Breadcrumb so the heartbeat/GCS report the module source even
        # though the build steps below are skipped.
        mkdir -p /run/ados 2>/dev/null || true
        printf 'dkms\n' > /run/ados/wfb-module-source 2>/dev/null || true
        exit 0
    fi
    info "${MODULE_NAME} is loaded but not a current patched DKMS build; rebuilding so the patched module is installed and reboot-persistent."
fi

# --- Prebuilt fast-path: load a verified prebuilt module, skip the compile --
#
# On-device DKMS compilation is slow and, on marginal hardware, can crash the
# compiler. Try a verified prebuilt .ko matched to this exact kernel first;
# any miss (no manifest, no match for this kernel, vermagic mismatch, failed
# verification, or failed load) falls through to the DKMS build below. Skip
# with ADOS_DRIVER_PREBUILT=0.
PREBUILT_LIB="${SCRIPT_DIR}/lib-prebuilt.sh"
if [ "${ADOS_DRIVER_PREBUILT:-1}" = "1" ] && [ -f "${PREBUILT_LIB}" ] && [ -n "${ARCH:-}" ]; then
    # shellcheck source=scripts/drivers/lib-prebuilt.sh disable=SC1091
    . "${PREBUILT_LIB}"
    if try_prebuilt_install "${MODULE_NAME}" "${KERNEL}" "${ARCH}"; then
        info "${MODULE_NAME} loaded from a verified prebuilt module (no on-device build)."
        exit 0
    fi
    info "No usable prebuilt ${MODULE_NAME}; building from source via DKMS."
fi

# --- Build the module from the vendored source via DKMS ---------------------

# DKMS needs the vendored source submodule.
if [ ! -d "${VENDOR_DIR}" ] || [ ! -f "${VENDOR_DIR}/dkms.conf" ]; then
    error "Vendor source not found at ${VENDOR_DIR}."
    error "Run: git submodule update --init --recursive"
    exit 1
fi

# Read version from dkms.conf
DRIVER_VERSION="$(awk -F'"' '/^PACKAGE_VERSION=/ {print $2}' "${VENDOR_DIR}/dkms.conf" | head -n1)"
if [ -z "${DRIVER_VERSION}" ]; then
    error "Could not parse PACKAGE_VERSION from ${VENDOR_DIR}/dkms.conf"
    exit 1
fi

# Read package name from dkms.conf
DKMS_PACKAGE="$(awk -F'"' '/^PACKAGE_NAME=/ {print $2}' "${VENDOR_DIR}/dkms.conf" | head -n1)"
if [ -z "${DKMS_PACKAGE}" ]; then
    error "Could not parse PACKAGE_NAME from ${VENDOR_DIR}/dkms.conf"
    exit 1
fi
DKMS_NAME="${DKMS_PACKAGE}/${DRIVER_VERSION}"
info "Driver: ${DKMS_NAME}"

# Install build deps if missing
NEED_INSTALL=""
command -v dkms >/dev/null 2>&1 || NEED_INSTALL="${NEED_INSTALL} dkms"
command -v make >/dev/null 2>&1 || NEED_INSTALL="${NEED_INSTALL} build-essential"
command -v patch >/dev/null 2>&1 || NEED_INSTALL="${NEED_INSTALL} patch"

# Pick the right headers package for the running distro
HEADERS_PKG=""
if [ -z "${NEED_INSTALL}" ] && [ -d "/lib/modules/${KERNEL}/build" ]; then
    info "Kernel headers present at /lib/modules/${KERNEL}/build."
else
    if apt-cache show "linux-headers-${KERNEL}" >/dev/null 2>&1; then
        HEADERS_PKG="linux-headers-${KERNEL}"
    elif apt-cache show "raspberrypi-kernel-headers" >/dev/null 2>&1; then
        HEADERS_PKG="raspberrypi-kernel-headers"
    elif apt-cache show "linux-headers-generic" >/dev/null 2>&1; then
        HEADERS_PKG="linux-headers-generic"
    else
        warn "No obvious headers package found for ${KERNEL}. Install manually if DKMS build fails."
    fi
fi

if [ -n "${NEED_INSTALL}" ] || [ -n "${HEADERS_PKG}" ]; then
    info "Installing build deps:${NEED_INSTALL} ${HEADERS_PKG}"
    apt-get update -qq
    # shellcheck disable=SC2086
    apt-get install -y ${NEED_INSTALL} ${HEADERS_PKG} || {
        error "apt-get install failed."
        exit 1
    }
fi

# Apply the mesh-enable patch before DKMS registers the source.
# Idempotent: patch -N skips the hunk if already applied.
if [ -f "${PATCH_FILE}" ]; then
    if grep -qxF "CONFIG_RTW_MESH = y" "${VENDOR_DIR}/Makefile"; then
        info "Mesh build flag already present in Makefile."
    else
        info "Applying mesh-enable patch to ${VENDOR_DIR}/Makefile"
        ( cd "${VENDOR_DIR}" && patch -p1 -N --forward < "${PATCH_FILE}" ) || {
            error "Patch application failed."
            exit 2
        }
    fi
else
    warn "Mesh-enable patch not found at ${PATCH_FILE}. 802.11s mesh mode will not be compiled in."
fi

# Suppress the spurious mlme-disconnect warning. rtw_mlmeext_disconnect()
# maps the adapter's role (AP/MESH/STA/ADHOC) to a disconnect action and
# hits a catch-all rtw_warn_on(1) for anything else. A monitor-mode
# interface (which is exactly how the radio link runs) has none of those
# roles, so every monitor-mode interface-down trips the warning even
# though the cleanup that follows is harmless. The patch adds an explicit
# monitor / no-link case so the path stays quiet. Applied before DKMS
# registers the source; idempotent (the marker line gates re-application).
MLME_PATCH_FILE="${REPO_ROOT}/data/driver-patches/monitor-disconnect-warn.patch"
MLME_SRC="${VENDOR_DIR}/core/rtw_mlme_ext.c"
if [ -f "${MLME_PATCH_FILE}" ] && [ -f "${MLME_SRC}" ]; then
    if grep -qF "MLME_IS_MONITOR(padapter) || MLME_IS_NULL(padapter)" "${MLME_SRC}"; then
        info "Monitor-mode disconnect patch already present."
    else
        info "Applying monitor-mode disconnect patch to ${MLME_SRC}"
        ( cd "${VENDOR_DIR}" && patch -p1 -N --forward < "${MLME_PATCH_FILE}" ) || {
            error "Monitor-mode disconnect patch application failed."
            exit 2
        }
    fi
else
    warn "Monitor-mode disconnect patch not found; the harmless mlme-disconnect warning will remain in dmesg."
fi

# ARCH was already exported near the top so both the prebuilt lookup and
# this DKMS build agree on the kernel's arch naming (arm64 / arm).

# The Pi 4B Trixie kernel and the Rock 5C BSP kernel both enable
# -Werror=misleading-indentation, -Werror=address-of-packed-member,
# and -Werror=date-time at KBUILD_CFLAGS scope. The vendored module
# source has all three patterns. KCFLAGS is overwritten by dkms in
# some versions so we route the relax flags via USER_EXTRA_CFLAGS in
# dkms.conf, which the module Makefile picks up at line 1 and appends
# to its own EXTRA_CFLAGS — the LAST -W flag wins, so -Wno-error
# overrides the kernel's promotion of these warnings. This patch must
# happen BEFORE dkms add because dkms copies the source at add time
# and never re-reads it.
RELAX_CFLAGS="-Wno-error -Wno-misleading-indentation -Wno-address-of-packed-member -Wno-date-time"
DKMS_CONF="${VENDOR_DIR}/dkms.conf"
if ! grep -q "USER_EXTRA_CFLAGS" "${DKMS_CONF}"; then
    info "Patching dkms.conf with relax cflags."
    sed -i.bak "s|^MAKE=\"'make' \(.*\)\"|MAKE=\"'make' \1 USER_EXTRA_CFLAGS='${RELAX_CFLAGS}'\"|" "${DKMS_CONF}"
fi

# Register source tree with DKMS. When the source is already registered
# we remove + re-add so updates to dkms.conf above take effect on the
# next build. dkms copies the source at `add` time and never re-reads
# it until the entry is removed.
if dkms status "${DKMS_PACKAGE}" 2>/dev/null | grep -q "${DRIVER_VERSION}"; then
    info "Refreshing existing DKMS source registration."
    dkms remove "${DKMS_NAME}" --all 2>/dev/null || true
    rm -rf "/var/lib/dkms/${DKMS_PACKAGE}/${DRIVER_VERSION}" 2>/dev/null || true
fi
# Always clear the copied source tree before re-adding. The package version
# is unchanged across source-patch revisions, so a stale /usr/src tree from
# an earlier build would otherwise be reused (dkms add does not overwrite an
# existing tree) and the freshly-patched source would never reach the build.
rm -rf "/usr/src/${DKMS_PACKAGE}-${DRIVER_VERSION}" 2>/dev/null || true

info "dkms add ${VENDOR_DIR}"
dkms add "${VENDOR_DIR}" || {
    error "dkms add failed."
    exit 2
}

# Keep the compile from pinning every core. On the small single-board
# computers this runs on, the only management link during a headless
# install is often a USB Wi-Fi dongle, and its driver shares the CPU
# with the build. When an all-core gcc run pins every core, the kernel
# cannot service the USB controller and the Wi-Fi link fast enough; the
# link drops and the board goes unreachable for the rest of the build
# with no kernel fault logged, so the install appears to freeze.
#
# DKMS picks its make -j from nproc and ignores framework.conf's
# parallel_jobs on some versions, so the documented knob is not enough
# on its own. The reliable cap is CPU affinity: confine the build (and
# every child gcc it spawns, since affinity is inherited) to a fixed two
# cores with taskset, leaving the rest fully free for the kernel's USB
# and network work. parallel_jobs is still set as a hint for DKMS
# versions that honor it, and the build is reniced. Both are best-effort
# and degrade gracefully when the tool or the knob is absent.
DKMS_FRAMEWORK="/etc/dkms/framework.conf"
if [ -f "${DKMS_FRAMEWORK}" ]; then
    if grep -qE '^[[:space:]]*parallel_jobs=' "${DKMS_FRAMEWORK}"; then
        sed -i.ados.bak "s|^[[:space:]]*parallel_jobs=.*|parallel_jobs=2|" "${DKMS_FRAMEWORK}"
    else
        cp -n "${DKMS_FRAMEWORK}" "${DKMS_FRAMEWORK}.ados.bak" 2>/dev/null || true
        printf 'parallel_jobs=2\n' >> "${DKMS_FRAMEWORK}"
    fi
fi

# Confine the build to two cores when the board has at least three, so
# the remaining cores stay free for the Wi-Fi link. Fall back to no
# confinement on dual/single-core boards or when taskset is unavailable.
BUILD_WRAP="nice -n 10"
_ncpu="$(nproc 2>/dev/null || echo 1)"
# Confine only when there are cores to spare AND taskset actually accepts the
# 0-1 mask on this box (probe it; a board with non-sequential core IDs or a
# locked-down cpuset could reject it, and a stale wrapper would then fail the
# build itself). Fall back to plain nice on any miss.
if [ "${_ncpu}" -ge 3 ] && command -v taskset >/dev/null 2>&1 && taskset -c 0-1 true >/dev/null 2>&1; then
    BUILD_WRAP="taskset -c 0-1 nice -n 10"
    info "Confining the Wi-Fi driver build to 2 of ${_ncpu} cores so the network link stays alive."
fi

# Build + install for current kernel (idempotent: dkms skips if already built)
info "Compiling the Wi-Fi driver against kernel ${KERNEL} (ARCH=${ARCH:-unset})."
info "This can take several minutes on this board. Build output follows."
# shellcheck disable=SC2086  # BUILD_WRAP is an intentional multi-word prefix
${BUILD_WRAP} dkms build "${DKMS_NAME}" -k "${KERNEL}" || {
    error "dkms build failed. See /var/lib/dkms/${DKMS_PACKAGE}/${DRIVER_VERSION}/build/make.log"
    exit 2
}

info "dkms install ${DKMS_NAME}"
dkms install "${DKMS_NAME}" -k "${KERNEL}" --force || {
    error "dkms install failed."
    exit 2
}

# Load the module
info "modprobe ${MODULE_NAME}"
modprobe "${MODULE_NAME}" || {
    error "modprobe ${MODULE_NAME} failed."
    exit 3
}

if ! lsmod | awk '{print $1}' | grep -qx "${MODULE_NAME}"; then
    error "${MODULE_NAME} not loaded after modprobe."
    exit 3
fi

# Breadcrumb so diagnostics + the GCS can report how the module was built.
mkdir -p /run/ados 2>/dev/null || true
printf 'dkms\n' > /run/ados/wfb-module-source 2>/dev/null || true

info "RTL8812EU driver installed and loaded with 802.11s mesh support."
exit 0

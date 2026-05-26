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

# Fast-path: if already loaded and installed, exit clean
if lsmod | awk '{print $1}' | grep -qx "${MODULE_NAME}"; then
    info "${MODULE_NAME} module already loaded."
    exit 0
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

info "dkms add ${VENDOR_DIR}"
dkms add "${VENDOR_DIR}" || {
    error "dkms add failed."
    exit 2
}

# Build + install for current kernel (idempotent: dkms skips if already built)
info "Compiling the Wi-Fi driver against kernel ${KERNEL} (ARCH=${ARCH:-unset})."
info "This can take several minutes on this board. Build output follows."
dkms build "${DKMS_NAME}" -k "${KERNEL}" || {
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

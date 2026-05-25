#!/usr/bin/env bash
# =============================================================================
# build-prebuilt-ko.sh — single source of build truth for the prebuilt
# RTL8812EU kernel module (8812eu.ko).
#
# Builds the vendored RTL8812EU driver out-of-tree against a specific
# installed kernel's headers, so a fresh install on a known kernel can load
# a verified prebuilt .ko instead of compiling from scratch via DKMS. The
# build steps here MUST stay byte-for-byte equivalent to the DKMS path in
# scripts/drivers/install-rtl8812eu.sh — same vendored source, same
# mesh-enable patch, same relax cflags — so a prebuilt module behaves
# identically to one DKMS would have produced.
#
# Used by:
#   - .github/workflows/driver-build.yml (CI, native ubuntu-24.04-arm)
#   - locally, against /lib/modules/<kver>/build of any installed kernel
#
# Usage:
#   sudo scripts/drivers/build-prebuilt-ko.sh <kernelrelease> <arch>
#
#   <kernelrelease>  the exact `uname -r`-style release whose headers live
#                    at /lib/modules/<kernelrelease>/build (do NOT guess it;
#                    derive it from the installed headers tree).
#   <arch>           arm64 (v1 only; armhf falls back to DKMS upstream).
#
# Output (stdout, last two lines, machine-parseable):
#   KO_PATH=<absolute path to the built 8812eu.ko>
#   VERMAGIC=<modinfo -F vermagic of that .ko>
#
# Exit codes:
#   0  success (8812eu.ko built and vermagic matches the target kernelrelease)
#   1  bad args / missing headers / missing vendor source
#   2  patch application failure
#   3  build failure or .ko not produced
#   4  vermagic mismatch (built module would not load on the target kernel)
# =============================================================================

set -euo pipefail

GREEN='\033[0;32m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
NC='\033[0m'

info()  { echo -e "${GREEN}[build-ko]${NC}  $*" >&2; }
warn()  { echo -e "${YELLOW}[build-ko]${NC}  $*" >&2; }
error() { echo -e "${RED}[build-ko]${NC}  $*" >&2; }

# Resolve repo root (script is at scripts/drivers/build-prebuilt-ko.sh).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
VENDOR_DIR="${REPO_ROOT}/vendor/rtl8812eu"
PATCH_FILE="${REPO_ROOT}/data/driver-patches/mesh-enable.patch"

# Module name exposed by the vendored driver build (matches dkms.conf
# BUILT_MODULE_NAME[0] and install-rtl8812eu.sh MODULE_NAME).
MODULE_NAME="8812eu"

# Relax cflags — kept identical to install-rtl8812eu.sh. The Pi 4B Trixie
# kernel and the Rock 5C BSP kernel promote -Wmisleading-indentation,
# -Waddress-of-packed-member and -Wdate-time to errors at KBUILD_CFLAGS
# scope; the vendored source trips all three. The LAST -W flag wins, so
# -Wno-error overrides the kernel's promotion of these warnings.
RELAX_CFLAGS="-Wno-error -Wno-misleading-indentation -Wno-address-of-packed-member -Wno-date-time"

KVER="${1:-}"
KARCH="${2:-}"

if [ -z "${KVER}" ] || [ -z "${KARCH}" ]; then
    error "usage: $0 <kernelrelease> <arch>"
    error "  e.g. $0 6.6.51+rpt-rpi-v8 arm64"
    exit 1
fi

# v1 builds arm64 only. armhf falls back to DKMS at install time.
if [ "${KARCH}" != "arm64" ]; then
    error "arch '${KARCH}' is not a prebuilt target; only arm64 is built (armhf falls back to DKMS)."
    exit 1
fi

KBUILD_DIR="/lib/modules/${KVER}/build"
if [ ! -d "${KBUILD_DIR}" ]; then
    error "kernel headers not found at ${KBUILD_DIR}."
    error "install the matching linux-headers package first."
    exit 1
fi

if [ ! -d "${VENDOR_DIR}" ] || [ ! -f "${VENDOR_DIR}/dkms.conf" ]; then
    error "vendor source not found at ${VENDOR_DIR}."
    error "run: git submodule update --init --recursive"
    exit 1
fi

info "kernelrelease: ${KVER}"
info "arch:          ${KARCH}"
info "kbuild dir:    ${KBUILD_DIR}"
info "vendor source: ${VENDOR_DIR}"

# Apply the mesh-enable patch before building, idempotently. patch -N
# --forward skips already-applied hunks. Mirrors install-rtl8812eu.sh.
if [ -f "${PATCH_FILE}" ]; then
    if grep -qxF "CONFIG_RTW_MESH = y" "${VENDOR_DIR}/Makefile"; then
        info "mesh build flag already present in Makefile."
    else
        info "applying mesh-enable patch to ${VENDOR_DIR}/Makefile"
        ( cd "${VENDOR_DIR}" && patch -p1 -N --forward < "${PATCH_FILE}" ) || {
            error "patch application failed."
            exit 2
        }
    fi
else
    warn "mesh-enable patch not found at ${PATCH_FILE}; 802.11s mesh mode will not be compiled in."
fi

# Clean any stale object tree from a prior build so the rebuild is
# deterministic against the requested kernel.
info "cleaning prior build artifacts"
make -C "${VENDOR_DIR}" clean >/dev/null 2>&1 || true
rm -f "${VENDOR_DIR}/${MODULE_NAME}.ko"

# Build out-of-tree against the target kernel. ARCH/KSRC/KDIR/KVER are
# passed explicitly so the build never falls back to `uname -r` or
# `uname -m` of the build host (the CI runner's own kernel differs from
# the target). The CONFIG_PLATFORM_* flags match the cross-build recipe:
# generic ARM platform, no I386, no RPi-specific tweaks.
info "building ${MODULE_NAME}.ko (this can take a few minutes)"
make -C "${VENDOR_DIR}" \
    ARCH=arm64 \
    KSRC="${KBUILD_DIR}" \
    KDIR="${KBUILD_DIR}" \
    KVER="${KVER}" \
    CONFIG_PLATFORM_I386_PC=n \
    CONFIG_PLATFORM_ARM_RPI=n \
    CONFIG_PLATFORM_ARM_GENERIC=y \
    USER_EXTRA_CFLAGS="${RELAX_CFLAGS}" \
    -j"$(nproc 2>/dev/null || echo 2)" || {
    error "make failed building ${MODULE_NAME}.ko"
    exit 3
}

KO_PATH="${VENDOR_DIR}/${MODULE_NAME}.ko"
if [ ! -f "${KO_PATH}" ]; then
    error "build did not produce ${KO_PATH}"
    exit 3
fi

# Read the module's vermagic and assert it begins with the target
# kernelrelease. modinfo -F vermagic emits e.g.
#   "6.6.51+rpt-rpi-v8 SMP preempt mod_unload aarch64"
# A built module only loads on a kernel whose vermagic matches, so a
# leading-token mismatch means we built against the wrong headers.
if ! command -v modinfo >/dev/null 2>&1; then
    error "modinfo not available; cannot assert vermagic"
    exit 3
fi

VERMAGIC="$(modinfo -F vermagic "${KO_PATH}" 2>/dev/null || true)"
if [ -z "${VERMAGIC}" ]; then
    error "could not read vermagic from ${KO_PATH}"
    exit 3
fi

VM_RELEASE="${VERMAGIC%% *}"
if [ "${VM_RELEASE}" != "${KVER}" ]; then
    error "vermagic mismatch: module built for '${VM_RELEASE}', expected '${KVER}'"
    error "full vermagic: ${VERMAGIC}"
    exit 4
fi

info "built ${MODULE_NAME}.ko for kernel ${KVER}"
info "vermagic: ${VERMAGIC}"

# Machine-parseable result on stdout (everything else went to stderr).
echo "KO_PATH=${KO_PATH}"
echo "VERMAGIC=${VERMAGIC}"
exit 0

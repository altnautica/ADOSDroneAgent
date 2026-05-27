#!/usr/bin/env bash
# build-prebuilt-ko.sh KVER OUTDIR: build the patched RTL8812EU module against
# the headers for KVER and stage the artifact for publishing.
#
# Run by .github/workflows/driver-build.yml AFTER the matrix row has installed
# that kernel flavor's exact headers (so /lib/modules/KVER/build exists). The
# headers' kernelrelease IS the KVER the caller passes in.
#
# Produces in OUTDIR (named by module-kver-arch so a manifest can pin them):
#   8812eu-<KVER>-<ARCH>.ko          the built module
#   8812eu-<KVER>-<ARCH>.ko.sha256   sha256sum -c input (hash  filename)
# and prints a single JSON object to stdout describing the artifact (module,
# kver, arch, vermagic, file, sha256) for the workflow to fold into the
# manifest. Signing happens in the workflow (it owns the secret), not here.
#
# Mirrors the on-device DKMS build's patch set + relax-cflags so the prebuilt
# is byte-for-byte the same module the device would have compiled.
set -euo pipefail

KVER="${1:?usage: build-prebuilt-ko.sh KVER OUTDIR}"
OUTDIR="${2:?usage: build-prebuilt-ko.sh KVER OUTDIR}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
VENDOR_DIR="${REPO_ROOT}/vendor/rtl8812eu"
MODULE_NAME="8812eu"
MESH_PATCH="${REPO_ROOT}/data/driver-patches/mesh-enable.patch"
MLME_PATCH="${REPO_ROOT}/data/driver-patches/monitor-disconnect-warn.patch"
KSRC="/lib/modules/${KVER}/build"

log() { printf '[build-ko] %s\n' "$*" >&2; }

[ -d "${VENDOR_DIR}" ] && [ -f "${VENDOR_DIR}/Makefile" ] || {
    log "vendor source missing at ${VENDOR_DIR} (run: git submodule update --init)"; exit 2; }
[ -d "${KSRC}" ] || { log "kernel headers absent at ${KSRC} for ${KVER}"; exit 2; }

# Kernel arch naming the module Makefile + kbuild expect (arm64, not aarch64).
case "$(uname -m)" in
    aarch64) ARCH=arm64 ;;
    armv6l|armv7l) ARCH=arm ;;
    x86_64) ARCH=x86_64 ;;
    *) ARCH="$(uname -m)" ;;
esac
export ARCH

# Build in a scratch copy so the submodule working tree stays clean and a
# re-run is deterministic.
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
cp -a "${VENDOR_DIR}/." "${WORK}/"

# Same patches the device applies, idempotent + forward so a re-run is safe.
if [ -f "${MESH_PATCH}" ] && ! grep -qxF "CONFIG_RTW_MESH = y" "${WORK}/Makefile"; then
    log "applying mesh-enable patch"
    ( cd "${WORK}" && patch -p1 -N --forward < "${MESH_PATCH}" )
fi
if [ -f "${MLME_PATCH}" ] && ! grep -qF "MLME_IS_MONITOR(padapter) || MLME_IS_NULL(padapter)" "${WORK}/core/rtw_mlme_ext.c"; then
    log "applying monitor-mode disconnect patch"
    ( cd "${WORK}" && patch -p1 -N --forward < "${MLME_PATCH}" )
fi

# Relax the warnings newer kernels promote to errors (matches the DKMS path).
RELAX_CFLAGS="-Wno-error -Wno-misleading-indentation -Wno-address-of-packed-member -Wno-date-time"

log "building ${MODULE_NAME} for ${KVER} (ARCH=${ARCH})"
make -C "${WORK}" -j"$(nproc)" \
    KVER="${KVER}" KSRC="${KSRC}" \
    USER_EXTRA_CFLAGS="${RELAX_CFLAGS}" \
    >&2

KO_SRC="${WORK}/${MODULE_NAME}.ko"
[ -f "${KO_SRC}" ] || { log "build produced no ${MODULE_NAME}.ko"; exit 3; }

VERMAGIC="$(modinfo -F vermagic "${KO_SRC}" 2>/dev/null || true)"
[ -n "${VERMAGIC}" ] || { log "could not read vermagic from the built module"; exit 3; }

mkdir -p "${OUTDIR}"
OUT_NAME="${MODULE_NAME}-${KVER}-${ARCH}.ko"
install -m 0644 "${KO_SRC}" "${OUTDIR}/${OUT_NAME}"
( cd "${OUTDIR}" && sha256sum "${OUT_NAME}" > "${OUT_NAME}.sha256" )
SHA="$(awk '{print $1}' "${OUTDIR}/${OUT_NAME}.sha256")"

log "built ${OUT_NAME} vermagic='${VERMAGIC}' sha256=${SHA}"
# One manifest row to stdout for the workflow to collect.
printf '{"module":"%s","kver":"%s","arch":"%s","vermagic":"%s","file":"%s","sha256":"%s"}\n' \
    "${MODULE_NAME}" "${KVER}" "${ARCH}" "${VERMAGIC}" "${OUT_NAME}" "${SHA}"

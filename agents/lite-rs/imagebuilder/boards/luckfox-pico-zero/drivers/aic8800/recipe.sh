#!/usr/bin/env bash
#
# Cross-build the AIC8800 USB Wi-Fi out-of-tree kernel modules against
# the Luckfox SDK kernel and a uclibc cross-toolchain. Also stage the
# proprietary firmware blobs the chip needs to associate.
#
# Args (positional):
#   $1 — kernel object dir (e.g. SDK_DIR/sysdrv/source/objs_kernel)
#   $2 — toolchain root    (e.g. SDK_DIR/tools/linux/toolchain/<triple>)
#   $3 — output dir        (e.g. SDK_DIR/build/aic8800)
#
# Emits:
#   <output-dir>/aic8800_fdrv.ko    main wireless driver
#   <output-dir>/aic_load_fw.ko     firmware loader
#   <output-dir>/firmware/aic8800DC/<blobs...>
#
# This script is invoked by the parent recipe::build_drivers hook and is
# expected to inherit the imgbuild:: helpers from the parent shell.

set -eu

KDIR="${1:?KDIR (kernel object dir) required as arg 1}"
TOOLCHAIN_DIR="${2:?TOOLCHAIN_DIR required as arg 2}"
OUT_DIR="${3:?OUT_DIR required as arg 3}"

# Pinned upstream — keep in sync with board.yaml.
AIC8800_REPO="https://github.com/radxa-pkg/aic8800"
AIC8800_REF="7f42b22913b462ab6c658dfc075bae1dbfe9a71a"

# Cross-toolchain triple matches the Luckfox SDK uclibc tree.
TOOLCHAIN_TRIPLE="arm-rockchip830-linux-uclibcgnueabihf"
TOOLCHAIN_PREFIX="${TOOLCHAIN_DIR}/bin/${TOOLCHAIN_TRIPLE}-"

# Source common helpers if running detached.
COMMON_SH="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../../../lib" && pwd)/common.sh"
# shellcheck disable=SC1090,SC1091
. "${COMMON_SH}"

mkdir -p "${OUT_DIR}"

if [ ! -d "${OUT_DIR}/src" ]; then
    imgbuild::log_step "Cloning aic8800 at ${AIC8800_REF}"
    git clone "${AIC8800_REPO}" "${OUT_DIR}/src"
    (
        cd "${OUT_DIR}/src"
        git checkout "${AIC8800_REF}"
    )
fi

# The upstream layout for this SHA places the USB driver tree at
# src/USB/driver_fw/drivers/aic8800 with sub-Makefiles for the firmware
# loader (aic_load_fw) and the main wireless driver (aic8800_fdrv).
DRV_SRC="${OUT_DIR}/src/src/USB/driver_fw/drivers/aic8800"
FW_SRC="${OUT_DIR}/src/src/USB/driver_fw/fw/aic8800DC"

if [ ! -d "${DRV_SRC}" ]; then
    imgbuild::log_error "aic8800 driver source not found at ${DRV_SRC}"
    # shellcheck disable=SC2317
    { return 1 2>/dev/null || exit 1; }
fi

# Disable the upstream Makefile's vendor platform branches; they hard-
# code Android-era cross-toolchain paths that do not exist in this
# build environment. Forcing PLATFORM_UBUNTU=y selects the generic
# branch where ARCH + CROSS_COMPILE come from the env, then we override
# KDIR explicitly.
imgbuild::log_step "Cross-building aic8800 against ${KDIR}"
make -C "${DRV_SRC}" \
    ARCH=arm \
    CROSS_COMPILE="${TOOLCHAIN_PREFIX}" \
    KDIR="${KDIR}" \
    CONFIG_PLATFORM_UBUNTU=y \
    CONFIG_PLATFORM_ROCKCHIP=n \
    CONFIG_PLATFORM_ALLWINNER=n \
    CONFIG_PLATFORM_AMLOGIC=n \
    CONFIG_PLATFORM_HI=n \
    -j"$(nproc 2>/dev/null || echo 2)"

# Collect both .ko files at the well-known output paths.
fdrv_ko="${DRV_SRC}/aic8800_fdrv/aic8800_fdrv.ko"
load_ko="${DRV_SRC}/aic_load_fw/aic_load_fw.ko"

if [ ! -f "${fdrv_ko}" ]; then
    imgbuild::log_error "aic8800 build did not produce aic8800_fdrv.ko at ${fdrv_ko}"
    # shellcheck disable=SC2317
    { return 1 2>/dev/null || exit 1; }
fi

cp -f "${fdrv_ko}" "${OUT_DIR}/aic8800_fdrv.ko"
imgbuild::log_ok "aic8800_fdrv .ko emitted at ${OUT_DIR}/aic8800_fdrv.ko"

if [ -f "${load_ko}" ]; then
    cp -f "${load_ko}" "${OUT_DIR}/aic_load_fw.ko"
    imgbuild::log_ok "aic_load_fw .ko emitted at ${OUT_DIR}/aic_load_fw.ko"
else
    imgbuild::log_warn "aic_load_fw.ko not produced — chip will not load firmware on first probe"
fi

# Stage the proprietary firmware blobs alongside the .ko files so the
# parent recipe's post_overlay hook can copy them into the rootfs.
if [ -d "${FW_SRC}" ]; then
    install -d -m 0755 "${OUT_DIR}/firmware/aic8800DC"
    cp -a "${FW_SRC}/." "${OUT_DIR}/firmware/aic8800DC/"
    imgbuild::log_ok "aic8800DC firmware staged at ${OUT_DIR}/firmware/aic8800DC/"
else
    imgbuild::log_warn "aic8800DC firmware blobs not found at ${FW_SRC}"
fi

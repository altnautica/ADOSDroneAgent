#!/usr/bin/env bash
#
# Cross-build the RTL8812EU out-of-tree kernel module against the
# Luckfox SDK kernel and a uclibc cross-toolchain.
#
# Args (positional):
#   $1 — kernel object dir (e.g. SDK_DIR/sysdrv/source/objs_kernel)
#   $2 — toolchain root    (e.g. SDK_DIR/tools/linux/toolchain/<triple>)
#   $3 — output dir        (e.g. SDK_DIR/build/rtl8812eu)
#
# Emits:
#   <output-dir>/88XXau.ko   the loadable kernel module
#
# This script is invoked by the parent recipe::build_drivers hook and is
# expected to inherit the imgbuild:: helpers from the parent shell.

set -eu

KDIR="${1:?KDIR (kernel object dir) required as arg 1}"
TOOLCHAIN_DIR="${2:?TOOLCHAIN_DIR required as arg 2}"
OUT_DIR="${3:?OUT_DIR required as arg 3}"

# Pinned upstream — keep in sync with board.yaml.
RTL8812EU_REPO="https://github.com/aircrack-ng/rtl8812au"
RTL8812EU_REF="7344855"

# Cross-toolchain triple matches the Luckfox SDK uclibc tree.
TOOLCHAIN_TRIPLE="arm-rockchip830-linux-uclibcgnueabihf"
TOOLCHAIN_PREFIX="${TOOLCHAIN_DIR}/bin/${TOOLCHAIN_TRIPLE}-"

# Source common helpers if running detached (lets shellcheck see them too).
COMMON_SH="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../../../lib" && pwd)/common.sh"
# shellcheck disable=SC1090,SC1091
. "${COMMON_SH}"

mkdir -p "${OUT_DIR}"

if [ ! -d "${OUT_DIR}/src" ]; then
    imgbuild::log_step "Cloning rtl8812au at ${RTL8812EU_REF}"
    git clone "${RTL8812EU_REPO}" "${OUT_DIR}/src"
    (
        cd "${OUT_DIR}/src"
        git checkout "${RTL8812EU_REF}"
    )
fi

# The upstream Makefile keys the build target on a pair of env vars
# (CONFIG_PLATFORM_I386_PC=n, CONFIG_PLATFORM_ARM_RPI=y / similar) plus
# the standard ARCH + CROSS_COMPILE pair. The 8812EU branch builds a
# single 88XXau.ko regardless of the platform define; we set the ARM
# generic platform flags so the build does not assume RPi-specific
# kernel tweaks.
imgbuild::log_step "Cross-building rtl8812eu against ${KDIR}"
make -C "${OUT_DIR}/src" \
    ARCH=arm \
    CROSS_COMPILE="${TOOLCHAIN_PREFIX}" \
    KSRC="${KDIR}" \
    KDIR="${KDIR}" \
    CONFIG_PLATFORM_I386_PC=n \
    CONFIG_PLATFORM_ARM_RPI=n \
    CONFIG_PLATFORM_ARM_GENERIC=y \
    -j"$(nproc 2>/dev/null || echo 2)"

# Copy the resulting .ko to the well-known output path the parent
# recipe expects.
src_ko="${OUT_DIR}/src/88XXau.ko"
if [ ! -f "${src_ko}" ]; then
    imgbuild::log_error "rtl8812eu build did not produce 88XXau.ko at ${src_ko}"
    # shellcheck disable=SC2317
    { return 1 2>/dev/null || exit 1; }
fi

cp -f "${src_ko}" "${OUT_DIR}/88XXau.ko"
imgbuild::log_ok "rtl8812eu .ko emitted at ${OUT_DIR}/88XXau.ko"

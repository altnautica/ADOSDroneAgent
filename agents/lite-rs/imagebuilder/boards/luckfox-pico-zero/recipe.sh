#!/usr/bin/env bash
#
# Luckfox Pico Zero board recipe.
#
# Drives the upstream LuckfoxTECH/luckfox-pico vendor SDK build for the
# RV1106G3 SoC, cross-builds the out-of-tree Wi-Fi modules against the
# SDK kernel, drops the lite agent binary + the RKMPI subprocess wrapper
# into the rootfs, and stages a flashable SD-card image.
#
# Sourced by imagebuilder/lib/common.sh::imgbuild::run_recipe. The
# orchestrator exports BOARD_DIR, SDK_DIR, OUTPUT_DIR, IMGBUILD_VERSION,
# REPO_ROOT, and friends.

set -eu

# Pinned upstream refs — keep in sync with board.yaml.
LUCKFOX_SDK_REF="824b817f889c"
LUCKFOX_LUNCH_TARGET="RV1106_Luckfox_Pico_Zero"

# Cross-toolchain shipped by the Luckfox SDK for the RV1106 uclibc target.
LUCKFOX_TOOLCHAIN_TRIPLE="arm-rockchip830-linux-uclibcgnueabihf"

# Path layout inside the SDK once cloned. Computed once and reused by
# every hook below so a relocation in a future SDK refresh is a one-line
# change.
recipe::_sdk_paths() {
    LUCKFOX_TOOLCHAIN_DIR="${SDK_DIR}/tools/linux/toolchain/${LUCKFOX_TOOLCHAIN_TRIPLE}"
    LUCKFOX_KERNEL_OBJ_DIR="${SDK_DIR}/sysdrv/source/objs_kernel"
    LUCKFOX_ROOTFS_DIR="${SDK_DIR}/output/out/rootfs_uclibc_rv1106"
    LUCKFOX_DRIVER_BUILD_DIR="${SDK_DIR}/build"
    LUCKFOX_IMAGE_OUT_DIR="${SDK_DIR}/output/image"
}

recipe::sdk_clone() {
    recipe::_sdk_paths

    imgbuild::log_info "Cloning Luckfox vendor SDK at ${LUCKFOX_SDK_REF}…"
    git clone https://github.com/LuckfoxTECH/luckfox-pico "${SDK_DIR}"
    (
        cd "${SDK_DIR}"
        git checkout "${LUCKFOX_SDK_REF}"
    )
}

recipe::sdk_configure() {
    recipe::_sdk_paths

    # The vendor SDK's choose_target_board() runs `ln -rfs` from
    # project/cfg/BoardConfig_IPC/<config>.mk to .BoardConfig.mk and
    # build.sh later guards on `[ -L "$BOARD_CONFIG" ] && source $BOARD_CONFIG`
    # at build.sh:2776 — meaning IF .BoardConfig.mk is not a symlink,
    # the SDK NEVER sources it and the kernel build fails with
    # "Not found toolchain -gcc for [] !!!" (RK_CHIP / RK_KERNEL_DTS /
    # toolchain prefix all unset). So a symlink is mandatory, not
    # optional. CI runs on ext4 — symlinks are fine. Local dev on
    # macOS / Linux is fine. WSL2 is fine. The earlier preemptive
    # switch to `cp` broke the toolchain dispatch.
    local board_cfg="${SDK_DIR}/project/cfg/BoardConfig_IPC/BoardConfig-EMMC-Buildroot-RV1106_Luckfox_Pico_Zero-IPC.mk"
    if [ ! -f "${board_cfg}" ]; then
        imgbuild::log_error "BoardConfig file not found at ${board_cfg}"
        find "${SDK_DIR}/project/cfg/BoardConfig_IPC/" -maxdepth 1 -type f 2>/dev/null | head -20 >&2
        return 1
    fi
    imgbuild::log_info "Symlinking .BoardConfig.mk → ${board_cfg##"${SDK_DIR}"/}"
    ln -rfs "${board_cfg}" "${SDK_DIR}/.BoardConfig.mk"
    if [ ! -L "${SDK_DIR}/.BoardConfig.mk" ]; then
        imgbuild::log_error "filesystem rejected the symlink — SDK build.sh:2776 requires \\`-L\\`"
        return 1
    fi

    # Vendor cross-toolchain — already extracted in the SDK tree.
    # The env_install_toolchain.sh script appends to ~/.bashrc which
    # doesn't help us in a non-interactive subshell. Just prepend
    # the bin dir explicitly. The toolchain lives nested one level
    # deeper than the toolchain root (under <toolchain-cross>/bin).
    export PATH="${LUCKFOX_TOOLCHAIN_DIR}/bin:${PATH}"
    if ! command -v arm-rockchip830-linux-uclibcgnueabihf-gcc >/dev/null 2>&1; then
        imgbuild::log_error "vendor toolchain not on PATH (looked in ${LUCKFOX_TOOLCHAIN_DIR}/bin)"
        find "${LUCKFOX_TOOLCHAIN_DIR}/bin" -maxdepth 1 -type f 2>/dev/null | head -10 >&2
        return 1
    fi

    # Patch the buildroot defconfig in place. The patch is a unified
    # diff against the upstream defconfig at the pinned SDK SHA; if the
    # SHA is bumped, regenerate the patch.
    imgbuild::log_info "Applying defconfig patch"
    patch -p1 -d "${SDK_DIR}" \
        < "${BOARD_DIR}/patches/0001-add-our-packages-to-defconfig.patch"
}

# build_drivers intentionally NOT defined for v0.1.
#
# AIC8800DC (the on-board Wi-Fi chip) ships in-tree at
# sysdrv/drv_ko/wifi/aic8800_netdrv/ and gets built by `./build.sh
# allsave` as part of the kernel modules — no out-of-tree work needed
# for the AP fallback path or any built-in Wi-Fi feature.
#
# RTL8812EU (the USB-attached dongle for WFB-ng broadcast) is operator-
# supplied hardware, only needed when WFB-ng is in use. The aircrack-ng
# v5.6.4.2 fork does NOT compile cleanly against the Luckfox 5.10 BSP
# kernel — its struct sta_info expectations (auth_len / pauth_frame),
# its NL80211_AUTHTYPE_SAE redefinition, and its `-Wno-stringop-overread`
# flag all clash with the older kernel + GCC 8.x toolchain the SDK
# pins. Sideloading the .ko post-flash via dkms or a separate package
# is the v0.1 path; v0.2 picks an upstream branch that's verified
# against kernel 5.10.

recipe::sdk_build() {
    recipe::_sdk_paths

    # The SDK builds the kernel as part of `./build.sh allsave`, plus
    # buildroot rootfs, U-Boot, and image pack — one shot.
    imgbuild::log_step "Building Luckfox SDK kernel / rootfs / U-Boot / image"
    (
        cd "${SDK_DIR}"
        # `allsave` is the SDK's documented one-shot — kernel + U-Boot +
        # buildroot rootfs + media + image pack. Faster than running
        # the stages individually because the SDK shares object caches
        # across stages internally.
        ./build.sh allsave
    )
}

recipe::pre_overlay() {
    recipe::_sdk_paths

    # Tell the orchestrator where the rootfs lives so the universal
    # overlay rsync lands in the right tree.
    ROOTFS_DIR="${LUCKFOX_ROOTFS_DIR}"
    export ROOTFS_DIR

    if [ ! -d "${ROOTFS_DIR}" ]; then
        imgbuild::log_error "expected rootfs at ${ROOTFS_DIR} but it does not exist"
        return 1
    fi

    # Pre-create the directories that the overlay drops files into so
    # rsync does not have to mkdir during the transfer.
    install -d -m 0755 \
        "${ROOTFS_DIR}/etc/ados" \
        "${ROOTFS_DIR}/usr/local/bin" \
        "${ROOTFS_DIR}/usr/lib/ados" \
        "${ROOTFS_DIR}/lib/firmware" \
        "${ROOTFS_DIR}/lib/modules"
}

recipe::post_overlay() {
    recipe::_sdk_paths

    # ---- agent binary --------------------------------------------------
    imgbuild::download_agent_binary \
        "armv7-unknown-linux-musleabihf" \
        "${ROOTFS_DIR}/usr/local/bin/ados-agent-lite"

    # ---- RKMPI wrapper -------------------------------------------------
    # The wrapper bridges the Rust agent to the vendor RKMPI hardware
    # encoder. Its own Makefile constructs CC from SDK_ROOT, so we
    # forward only that and let the inner Makefile pick its toolchain.
    local rkmpi_src="${REPO_ROOT}/agents/lite-rs/boards/luckfox-pico-zero/rkmpi-wrapper"
    if [ -f "${rkmpi_src}/Makefile" ]; then
        imgbuild::log_step "Cross-building RKMPI wrapper"
        make -C "${rkmpi_src}" SDK_ROOT="${SDK_DIR}"
        install -D -m 0755 \
            "${rkmpi_src}/rkmpi-wrapper" \
            "${ROOTFS_DIR}/usr/lib/ados/rkmpi-wrapper"
    else
        imgbuild::log_warn "RKMPI wrapper source not found — skipping"
    fi

    # ---- kernel modules ------------------------------------------------
    # The kernel.release file under the configured kernel objects tree
    # gives us KVER for placing modules at the path the loader expects.
    local kver
    if [ -r "${LUCKFOX_KERNEL_OBJ_DIR}/include/config/kernel.release" ]; then
        kver=$(cat "${LUCKFOX_KERNEL_OBJ_DIR}/include/config/kernel.release")
    else
        imgbuild::log_warn "kernel.release missing; falling back to 'unknown' KVER"
        kver="unknown"
    fi

    local rtl_ko="${LUCKFOX_DRIVER_BUILD_DIR}/rtl8812eu/88XXau.ko"
    if [ -f "${rtl_ko}" ]; then
        install -D -m 0644 "${rtl_ko}" \
            "${ROOTFS_DIR}/lib/modules/${kver}/extra/88XXau.ko"
    else
        imgbuild::log_warn "rtl8812eu .ko missing at ${rtl_ko} — adapter will not load on first boot"
    fi

    local aic_ko="${LUCKFOX_DRIVER_BUILD_DIR}/aic8800/aic8800_fdrv.ko"
    if [ -f "${aic_ko}" ]; then
        install -D -m 0644 "${aic_ko}" \
            "${ROOTFS_DIR}/lib/modules/${kver}/extra/aic8800_fdrv.ko"
    else
        imgbuild::log_warn "aic8800 .ko missing at ${aic_ko} — on-board Wi-Fi will not associate"
    fi

    local aic_load_ko="${LUCKFOX_DRIVER_BUILD_DIR}/aic8800/aic_load_fw.ko"
    if [ -f "${aic_load_ko}" ]; then
        install -D -m 0644 "${aic_load_ko}" \
            "${ROOTFS_DIR}/lib/modules/${kver}/extra/aic_load_fw.ko"
    fi

    # ---- firmware blobs (copied by the aic8800 driver recipe) ---------
    local aic_fw_src="${LUCKFOX_DRIVER_BUILD_DIR}/aic8800/firmware/aic8800DC"
    if [ -d "${aic_fw_src}" ]; then
        install -d -m 0755 "${ROOTFS_DIR}/lib/firmware/aic8800DC"
        cp -a "${aic_fw_src}/." "${ROOTFS_DIR}/lib/firmware/aic8800DC/"
    fi

    # ---- init system reconciliation ----------------------------------
    # The universal overlay ships both a busybox sysv-rc init script and
    # a systemd unit. Luckfox runs busybox sysv-rc, so the systemd unit
    # is dead weight — strip it.
    rm -f "${ROOTFS_DIR}/etc/systemd/system/ados-agent-lite.service" 2>/dev/null || true
    rmdir "${ROOTFS_DIR}/etc/systemd/system" 2>/dev/null || true
    rmdir "${ROOTFS_DIR}/etc/systemd" 2>/dev/null || true
}

recipe::stage_image() {
    recipe::_sdk_paths

    # The vendor pipeline emits a flashable SD-card image at one of:
    #   ${SDK_DIR}/output/image/SD_update.img
    #   ${SDK_DIR}/output/image/update.img
    # plus a few component images. Pick SD_update.img if present, else
    # the first .img we find.
    local img_src=""
    if [ -f "${LUCKFOX_IMAGE_OUT_DIR}/SD_update.img" ]; then
        img_src="${LUCKFOX_IMAGE_OUT_DIR}/SD_update.img"
    else
        img_src=$(find "${LUCKFOX_IMAGE_OUT_DIR}" -maxdepth 1 -type f -name '*.img' 2>/dev/null | head -n1)
    fi

    if [ -z "${img_src}" ] || [ ! -f "${img_src}" ]; then
        imgbuild::log_error "Luckfox build did not produce an .img"
        ls -lah "${LUCKFOX_IMAGE_OUT_DIR}/" || true
        return 1
    fi

    local artifact="${OUTPUT_DIR}/ados-${BOARD_SLUG}-${VERSION}.img.gz"
    imgbuild::log_step "Compressing ${img_src} -> ${artifact}"
    gzip -c "${img_src}" > "${artifact}"

    (
        cd "${OUTPUT_DIR}"
        sha256sum "$(basename "${artifact}")" > "$(basename "${artifact}").sha256"
    )
}

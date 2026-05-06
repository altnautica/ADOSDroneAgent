#!/usr/bin/env bash
#
# Recipe for the Raspberry Pi Zero 2 W.
#
# Drives the upstream RPi-Distro/pi-gen image builder to produce a
# Debian Bookworm Lite-class aarch64 image, then mounts the resulting
# .img and lays down the universal ADOS overlay (agent binary, AP
# fallback config, systemd unit) before repacking + signing.
#
# pi-gen layout reminder:
#   stage0  = bootstrap
#   stage1  = base system
#   stage2  = Lite (this is what we ship)
#   stage3  = X11 desktop          \
#   stage4  = full desktop          > skipped — we are headless
#   stage5  = recommended apps     /
#
# Branch:   bookworm-arm64        — pre-pinned to bookworm + ARCH=arm64
# Output:   ${SDK_DIR}/deploy/<date>-ados-pi-zero-2w-lite.img.gz

# shellcheck shell=bash
# shellcheck disable=SC2034  # IMG_SRC, WORK_IMG, ROOTFS_DIR, LOOPDEV are
                             # written here and re-read in later hooks via
                             # files in $SDK_DIR; they are not unused.

PI_GEN_BRANCH="bookworm-arm64"
PI_GEN_REPO="https://github.com/RPi-Distro/pi-gen"
TARGET_TRIPLE="aarch64-unknown-linux-gnu"
IMG_BASENAME="ados-pi-zero-2w"

recipe::sdk_clone() {
    git clone --depth 1 --branch "${PI_GEN_BRANCH}" "${PI_GEN_REPO}" "${SDK_DIR}"
}

recipe::sdk_configure() {
    # pi-gen sources `${SDK_DIR}/config` at build start. ARCH=arm64 and
    # RELEASE=bookworm come from the bookworm-arm64 branch defaults; we
    # only override what we need to differ from upstream.
    cat > "${SDK_DIR}/config" <<EOF
IMG_NAME=${IMG_BASENAME}
DEPLOY_COMPRESSION=gz
COMPRESSION_LEVEL=6
# SSH is disabled at first boot. The agent's setup webapp at
# port 8080 owns onboarding; SSH gets enabled (with an operator-
# supplied key) from the webapp once the device is paired.
# PUBKEY_ONLY_SSH was tempting but requires PUBKEY_SSH_FIRST_USER to
# be a real key — pi-gen aborts otherwise — so leave it off.
ENABLE_SSH=0
LOCALE_DEFAULT=en_GB.UTF-8
TARGET_HOSTNAME=ados
DISABLE_FIRST_BOOT_USER_RENAME=1
FIRST_USER_NAME=ados
# pi-gen requires FIRST_USER_PASS to be set explicitly. The ADOS
# image ships a default password the operator is REQUIRED to change
# on first pair via the agent's setup webapp; the bench runbook
# documents this. Sourcing the password from a CI secret would be
# cleaner long-term but the default-then-rotate flow keeps the
# rolling-image build reproducible.
FIRST_USER_PASS=ados-default-change-me
EOF

    # Headless image — drop the desktop and recommended-apps stages
    # entirely, both from execution (SKIP) and from final image export
    # (SKIP_IMAGES).
    local stage
    for stage in stage3 stage4 stage5; do
        touch "${SDK_DIR}/${stage}/SKIP" "${SDK_DIR}/${stage}/SKIP_IMAGES"
    done

    # Layer our own stage2 sub-stage on top to pull in the userspace
    # packages the lite agent supervises (hostapd / dnsmasq / iw /
    # wireless-regdb / chrony) and to pre-create /etc/ados directories.
    if [ -d "${BOARD_DIR}/stage-overrides" ]; then
        cp -a "${BOARD_DIR}/stage-overrides/." "${SDK_DIR}/"
    fi
}

recipe::sdk_build() {
    # pi-gen requires root for chroot + losetup + parted.
    ( cd "${SDK_DIR}" && sudo ./build.sh )
}

recipe::pre_overlay() {
    # pi-gen leaves the final image as a gzipped artifact in
    # ${SDK_DIR}/deploy/. We gunzip a working copy, mount its rootfs
    # partition, and hand ROOTFS_DIR to the orchestrator.
    local img_src=""
    local candidate
    while IFS= read -r -d '' candidate; do
        img_src="${candidate}"
        break
    done < <(find "${SDK_DIR}/deploy" -maxdepth 1 -type f \
        -name "*-${IMG_BASENAME}*.img.gz" -print0 2>/dev/null)
    if [ -z "${img_src}" ] || [ ! -f "${img_src}" ]; then
        imgbuild::log_error "pi-gen did not produce a deploy/*-${IMG_BASENAME}*.img.gz"
        return 1
    fi

    local work_img="${SDK_DIR}/work/${IMG_BASENAME}.img"
    mkdir -p "$(dirname "${work_img}")"
    gunzip -c "${img_src}" > "${work_img}"

    # Two-partition layout: p1 = vfat boot, p2 = ext4 rootfs. Mount p2.
    ROOTFS_DIR=$(mktemp -d -t "ados-${BOARD_SLUG}-rootfs-XXXXXX")
    local loopdev
    loopdev=$(sudo losetup -f --show -P "${work_img}")
    sudo mount "${loopdev}p2" "${ROOTFS_DIR}"

    # The orchestrator's overlay_into() runs `rsync -a` as the build
    # user, but pi-gen's rootfs is owned by root:root. Temporarily make
    # the directories the universal overlay writes into traversable +
    # writable by the build user. post_overlay restores ownership +
    # mode so the published image stays correctly owned.
    local d
    for d in etc etc/ados etc/ados/ap-fallback etc/init.d \
             etc/systemd etc/systemd/system; do
        sudo install -d -m 0777 "${ROOTFS_DIR}/${d}"
    done

    # Stash bookkeeping for post_overlay + stage_image. Using files
    # under ${SDK_DIR} keeps the values across hook boundaries without
    # needing a sourced env file.
    printf '%s\n' "${loopdev}"  > "${SDK_DIR}/.ados-loopdev"
    printf '%s\n' "${work_img}" > "${SDK_DIR}/.ados-workimg"

    export ROOTFS_DIR
}

recipe::post_overlay() {
    # Restore ownership + mode on the dirs we relaxed in pre_overlay
    # plus everything the overlay rsync created inside them. /etc/ in
    # particular is mode 0755 root:root upstream and we want to keep
    # that contract.
    local d
    for d in etc etc/init.d etc/systemd etc/systemd/system; do
        sudo chown root:root "${ROOTFS_DIR}/${d}"
        sudo chmod 0755 "${ROOTFS_DIR}/${d}"
    done
    sudo chown -R root:root "${ROOTFS_DIR}/etc/ados"
    sudo chmod -R u=rwX,go=rX "${ROOTFS_DIR}/etc/ados"
    if [ -f "${ROOTFS_DIR}/etc/systemd/system/ados-agent-lite.service" ]; then
        sudo chown root:root "${ROOTFS_DIR}/etc/systemd/system/ados-agent-lite.service"
        sudo chmod 0644 "${ROOTFS_DIR}/etc/systemd/system/ados-agent-lite.service"
    fi

    # Drop the agent binary into /usr/local/bin. aarch64 + glibc.
    # The rootfs is mounted root-owned, so we stage the download in a
    # build-user-writable tmpdir first and `sudo install` it across.
    local stage_bin
    stage_bin=$(mktemp -d -t "ados-${BOARD_SLUG}-bin-XXXXXX")
    imgbuild::download_agent_binary \
        "${TARGET_TRIPLE}" \
        "${stage_bin}/ados-agent-lite"
    sudo install -d -m 0755 -o root -g root "${ROOTFS_DIR}/usr/local/bin"
    sudo install -m 0755 -o root -g root \
        "${stage_bin}/ados-agent-lite" \
        "${ROOTFS_DIR}/usr/local/bin/ados-agent-lite"
    rm -rf "${stage_bin}"

    # systemd-class board: the busybox sysv-rc init script that the
    # universal overlay shipped is dead weight here. Drop it.
    sudo rm -f "${ROOTFS_DIR}/etc/init.d/S99ados-agent-lite"

    # Enable the systemd unit by hand. We avoid `chroot ... systemctl
    # enable` because (a) it drags in qemu-user-static plumbing and (b)
    # `systemctl enable` is a thin wrapper around the symlink we are
    # creating directly.
    sudo mkdir -p "${ROOTFS_DIR}/etc/systemd/system/multi-user.target.wants"
    sudo ln -sf /etc/systemd/system/ados-agent-lite.service \
        "${ROOTFS_DIR}/etc/systemd/system/multi-user.target.wants/ados-agent-lite.service"

    # Tear down the loop mount. The image file under ${SDK_DIR}/work
    # stays put so stage_image can repack it.
    local loopdev work_img
    loopdev=$(cat "${SDK_DIR}/.ados-loopdev")
    work_img=$(cat "${SDK_DIR}/.ados-workimg")
    sudo sync
    sudo umount "${ROOTFS_DIR}"
    sudo losetup -d "${loopdev}"
    rmdir "${ROOTFS_DIR}"
    unset ROOTFS_DIR
    : "${work_img}"  # silence shellcheck — used by stage_image
}

recipe::stage_image() {
    local work_img artifact
    work_img=$(cat "${SDK_DIR}/.ados-workimg")
    if [ ! -f "${work_img}" ]; then
        imgbuild::log_error "no working image at ${work_img} — pre_overlay/post_overlay broke the chain"
        return 1
    fi

    artifact="${OUTPUT_DIR}/ados-${BOARD_SLUG}-${VERSION}.img.gz"
    mkdir -p "${OUTPUT_DIR}"
    gzip -c -6 "${work_img}" > "${artifact}"

    ( cd "${OUTPUT_DIR}" && sha256sum "$(basename "${artifact}")" > "$(basename "${artifact}").sha256" )
    imgbuild::log_ok "staged ${artifact}"
}

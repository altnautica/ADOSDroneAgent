#!/bin/bash -e
#
# pi-gen runs this from the host (not inside the chroot). ${ROOTFS_DIR}
# points at the in-progress image rootfs. We pre-create the directory
# tree the lite agent + its supervised helpers expect so the universal
# overlay rsync in the next phase lands on top of an already-correct
# layout.

install -d -m 0755 -o root -g root "${ROOTFS_DIR}/etc/ados"
install -d -m 0755 -o root -g root "${ROOTFS_DIR}/etc/ados/ap-fallback"
install -d -m 0755 -o root -g root "${ROOTFS_DIR}/var/lib/ados"
install -d -m 0755 -o root -g root "${ROOTFS_DIR}/var/log/ados"
install -d -m 0755 -o root -g root "${ROOTFS_DIR}/run/ados"

# Make sure the services that ship enabled by default in Debian for
# brcm-firmware-fed Wi-Fi (wpa_supplicant, NetworkManager) do not fight
# with the agent's own AP-fallback supervisor at first boot. We DO NOT
# disable them here — the agent decides at runtime which one to drive.
# This file just stakes out the directory layout.

exit 0

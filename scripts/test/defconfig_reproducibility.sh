#!/usr/bin/env bash
# =============================================================================
# Defconfig reproducibility smoke test.
#
# Validates that the Luckfox Pico Zero defconfig is byte-stable on disk:
# the same file hashes the same on two consecutive reads, and the file
# itself is non-empty. This is the lightweight half of "two clean
# Buildroot builds produce identical .config" — a stricter test that
# requires Buildroot host tools and lives in the dedicated image-build
# CI workflow.
# =============================================================================

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DEFCONFIG="${REPO_ROOT}/agents/lite-rs/buildroot/configs/luckfox_pico_zero_ados_defconfig"

if [ ! -f "${DEFCONFIG}" ]; then
    echo "missing defconfig: ${DEFCONFIG}" >&2
    exit 1
fi

# Reject empty files. A zero-byte defconfig would still hash equally
# on two reads but is obviously broken.
if [ ! -s "${DEFCONFIG}" ]; then
    echo "defconfig is empty: ${DEFCONFIG}" >&2
    exit 1
fi

# Pick a SHA256 helper that exists on both Linux runners (sha256sum)
# and macOS dev hosts (shasum). Both emit "<hash>  <path>".
sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        echo "no sha256 helper available (install coreutils)" >&2
        exit 127
    fi
}

HASH1="$(sha256_of "${DEFCONFIG}")"
HASH2="$(sha256_of "${DEFCONFIG}")"

if [ "${HASH1}" != "${HASH2}" ]; then
    echo "defconfig hashes differ between consecutive reads:" >&2
    echo "  read 1: ${HASH1}" >&2
    echo "  read 2: ${HASH2}" >&2
    exit 1
fi

# Spot-check a few invariants. The ADOS defconfig is an additive delta
# on top of the upstream stock Luckfox defconfig; the toolchain and
# rootfs selections come from upstream. We pin the deltas we own:
# the agent package, the rootfs overlay, and the two Wi-Fi drivers.
# If any of these get dropped during a future edit, CI catches the
# mistake before the image ever boots.
required_keys=(
    "BR2_PACKAGE_ADOS_AGENT_LITE=y"
    "BR2_PACKAGE_RTL8812EU=y"
    "BR2_PACKAGE_AIC8800=y"
    "BR2_PACKAGE_MINISIGN=y"
    "BR2_ROOTFS_OVERLAY="
)
for key in "${required_keys[@]}"; do
    if ! grep -q "^${key}" "${DEFCONFIG}"; then
        echo "defconfig is missing required key: ${key}" >&2
        exit 1
    fi
done

echo "ok: defconfig stable, sha256 ${HASH1}"

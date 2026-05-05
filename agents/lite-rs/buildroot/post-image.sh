#!/bin/sh
#
# post-image.sh — finish a Buildroot run by gzip-compressing the
# resulting microSD image and emitting a SHA256 checksum file.
#
# Inputs:
#   $1  Path to the raw `.img` produced by Buildroot at
#       output/images/sdcard.img (or whatever the SDK names it).
#
# Outputs (alongside the input):
#   <input>.gz         gzip -9 -k of the input (original kept)
#   <input>.gz.sha256  sha256sum of the gzipped artifact
#
# minisign signing happens in CI after this script exits — the keypair
# lives in repository secrets and never touches the build host. This
# script is intentionally signing-free so it can run unprivileged on
# any developer machine that wants to re-run the gzip+sha step locally.

set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <path-to-img>" >&2
    exit 2
fi

IMG="$1"

if [ ! -f "$IMG" ]; then
    echo "post-image: input file not found: $IMG" >&2
    exit 1
fi

# -k preserves the original .img (test harnesses that want to re-flash
# from raw don't have to undo the compression).
gzip -9 -k -f "$IMG"

# Emit a sibling sha256 so consumers can verify the gzipped artifact
# without having to re-run the whole pipeline.
sha256sum "${IMG}.gz" > "${IMG}.gz.sha256"

echo "post-image: ${IMG}.gz ready"

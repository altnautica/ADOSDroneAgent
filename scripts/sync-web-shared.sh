#!/usr/bin/env bash
# Copy the canonical shared web code into both agent SPAs.
#
#   scripts/sync-web-shared.sh          copy web-shared/ into each app
#   scripts/sync-web-shared.sh --check  exit 1 when a copy drifted
#
# Sources: web-shared/src/ and web-shared/tailwind-preset.cjs.
# Targets: cockpit/src/shared/ and dashboard/src/shared/.
#
# Plain copies rather than a package link: the copies are ordinary files, so
# each app builds on its own and the CI drift check is a byte comparison.

set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
src_dir="${repo_root}/web-shared/src"
preset="${repo_root}/web-shared/tailwind-preset.cjs"
apps=(cockpit dashboard)

mode="sync"
case "${1:-}" in
    "") ;;
    --check) mode="check" ;;
    *)
        echo "usage: $0 [--check]" >&2
        exit 2
        ;;
esac

if [ ! -d "${src_dir}" ] || [ ! -f "${preset}" ]; then
    echo "[sync-web-shared] missing ${src_dir} or ${preset}" >&2
    exit 1
fi

# The expected tree for one app, built in a temp dir so --check compares the
# exact layout a sync would write.
stage="$(mktemp -d)"
trap 'rm -rf "${stage}"' EXIT
cp -R "${src_dir}/." "${stage}/"
cp "${preset}" "${stage}/tailwind-preset.cjs"

drift=0
for app in "${apps[@]}"; do
    app_dir="${repo_root}/${app}"
    target="${app_dir}/src/shared"
    if [ ! -d "${app_dir}/src" ]; then
        echo "[sync-web-shared] ${app}/src not found" >&2
        exit 1
    fi
    if [ "${mode}" = "check" ]; then
        if ! diff -r "${stage}" "${target}" >/dev/null 2>&1; then
            echo "[sync-web-shared] ${app}/src/shared differs from web-shared:" >&2
            diff -r "${stage}" "${target}" >&2 || true
            drift=1
        fi
    else
        rm -rf "${target}"
        mkdir -p "${target}"
        cp -R "${stage}/." "${target}/"
        echo "[sync-web-shared] synced ${app}/src/shared"
    fi
done

if [ "${drift}" -ne 0 ]; then
    echo "[sync-web-shared] run scripts/sync-web-shared.sh and commit the result." >&2
    exit 1
fi

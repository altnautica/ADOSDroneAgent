#!/usr/bin/env bash
# Build the on-screen ground-station cockpit SPA and stage the output where
# the Python wheel can pick it up.
#
# Local dev usage:
#   scripts/build-cockpit.sh
#
# CI usage (run before `uv build --wheel`):
#   scripts/build-cockpit.sh
#   uv build --wheel
#
# The Vite build outputs to ADOSDroneAgent/cockpit/dist/. This script clears
# the staging path src/ados/cockpit/static/ and copies the build output in so
# setuptools picks it up via the ados.cockpit package-data glob in
# pyproject.toml. This is a separate bundle from the laptop dashboard
# (scripts/build-dashboard.sh); touching one does not require the other.
#
# Exits 0 on success, non-zero on failure.

set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cockpit_src="${repo_root}/cockpit"
stage_dir="${repo_root}/src/ados/cockpit/static"

if [ ! -d "${cockpit_src}" ]; then
    echo "[build-cockpit] cockpit/ directory not found at ${cockpit_src}" >&2
    exit 1
fi

if ! command -v npm >/dev/null 2>&1; then
    echo "[build-cockpit] npm is required to build the cockpit but was not found on PATH." >&2
    exit 1
fi

echo "[build-cockpit] installing dependencies"
( cd "${cockpit_src}" && npm ci --no-audit --no-fund )

echo "[build-cockpit] building production bundle"
( cd "${cockpit_src}" && npm run build )

if [ ! -f "${cockpit_src}/dist/index.html" ]; then
    echo "[build-cockpit] vite build did not produce dist/index.html" >&2
    exit 1
fi

echo "[build-cockpit] staging dist into ${stage_dir}"
rm -rf "${stage_dir}"
mkdir -p "${stage_dir}"
cp -R "${cockpit_src}/dist/." "${stage_dir}/"

echo "[build-cockpit] done. ${stage_dir} now contains:"
ls -la "${stage_dir}" | head -20

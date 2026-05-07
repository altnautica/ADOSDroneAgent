#!/usr/bin/env bash
# Build the browser dashboard SPA and stage the output where the
# Python wheel can pick it up.
#
# Local dev usage:
#   scripts/build-dashboard.sh
#
# CI usage (run before `uv build --wheel`):
#   scripts/build-dashboard.sh
#   uv build --wheel
#
# The Vite build outputs to ADOSDroneAgent/dashboard/dist/. This script
# clears the staging path src/ados/dashboard/dist/ and copies the build
# output in so setuptools picks it up via the ados.dashboard
# package-data glob in pyproject.toml.
#
# Exits 0 on success, non-zero on failure.

set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
dashboard_src="${repo_root}/dashboard"
stage_dir="${repo_root}/src/ados/dashboard/static"

if [ ! -d "${dashboard_src}" ]; then
    echo "[build-dashboard] dashboard/ directory not found at ${dashboard_src}" >&2
    exit 1
fi

if ! command -v npm >/dev/null 2>&1; then
    echo "[build-dashboard] npm is required to build the dashboard but was not found on PATH." >&2
    exit 1
fi

echo "[build-dashboard] installing dependencies"
( cd "${dashboard_src}" && npm ci --no-audit --no-fund )

echo "[build-dashboard] building production bundle"
( cd "${dashboard_src}" && npm run build )

if [ ! -f "${dashboard_src}/dist/index.html" ]; then
    echo "[build-dashboard] vite build did not produce dist/index.html" >&2
    exit 1
fi

echo "[build-dashboard] staging dist into ${stage_dir}"
rm -rf "${stage_dir}"
mkdir -p "${stage_dir}"
cp -R "${dashboard_src}/dist/." "${stage_dir}/"

echo "[build-dashboard] done. ${stage_dir} now contains:"
ls -la "${stage_dir}" | head -20

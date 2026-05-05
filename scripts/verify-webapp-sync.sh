#!/usr/bin/env bash
# verify-webapp-sync.sh
#
# Checks that the canonical universal setup webapp (`web/setup/`) and the
# Rust lite-agent's embedded copy (`agents/lite-rs/crates/ados-setup/web-setup/`)
# agree byte-for-byte. The Rust crate uses `include_dir!` against a relative
# path inside the crate, which means a stale embedded copy ships in the
# binary while the Python full agent serves the up-to-date canonical copy
# — operators see two different wizards depending on which agent half is
# running. This script catches that drift.
#
# Exit codes:
#   0 - canonical and embedded copies match
#   1 - drift detected (script prints which files differ + how to sync)
#   2 - script invariant violated (missing directory, missing tools)

set -eu

CANONICAL_DIR="web/setup"
EMBEDDED_DIR="agents/lite-rs/crates/ados-setup/web-setup"

# Anchor on the repo root so the script works whether invoked from CI,
# from inside the agent submodule, or by hand.
if [ -d ".git" ]; then
    REPO_ROOT="$(pwd)"
elif git rev-parse --show-toplevel >/dev/null 2>&1; then
    REPO_ROOT="$(git rev-parse --show-toplevel)"
else
    echo "verify-webapp-sync: not inside a git checkout" >&2
    exit 2
fi
cd "${REPO_ROOT}"

if [ ! -d "${CANONICAL_DIR}" ]; then
    echo "verify-webapp-sync: missing canonical directory ${CANONICAL_DIR}" >&2
    exit 2
fi
if [ ! -d "${EMBEDDED_DIR}" ]; then
    echo "verify-webapp-sync: missing embedded directory ${EMBEDDED_DIR}" >&2
    echo "  expected the Rust agent's include_dir! source to live there" >&2
    exit 2
fi

# Use a portable hash. macOS has shasum; Linux has sha256sum.
if command -v sha256sum >/dev/null 2>&1; then
    HASHER="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
    HASHER="shasum -a 256"
else
    echo "verify-webapp-sync: neither sha256sum nor shasum on PATH" >&2
    exit 2
fi

# Compare *.html, *.css, *.js, *.svg, *.png, *.jpg. Skip __pycache__ and
# __init__.py — those are Python packaging artefacts that exist only in
# the canonical tree. Build a sorted hash listing of each tree (relative
# to that tree's root) so a diff highlights both content drift and
# missing files.
hash_tree() {
    local root="$1"
    ( cd "${root}" && find . -type f \
        \( -name '*.html' -o -name '*.css' -o -name '*.js' \
           -o -name '*.svg' -o -name '*.png' -o -name '*.jpg' \) \
        -not -path '*/__pycache__/*' \
        | sort \
        | while read -r f; do
            ${HASHER} "${f}" | awk '{printf "%s %s\n", $1, $2}'
          done
    )
}

CANONICAL_HASHES="$(hash_tree "${CANONICAL_DIR}")"
EMBEDDED_HASHES="$(hash_tree "${EMBEDDED_DIR}")"

if [ "${CANONICAL_HASHES}" = "${EMBEDDED_HASHES}" ]; then
    file_count="$(echo "${CANONICAL_HASHES}" | wc -l | tr -d ' ')"
    echo "verify-webapp-sync: ok, ${file_count} files match"
    exit 0
fi

echo "verify-webapp-sync: DRIFT DETECTED" >&2
echo "" >&2
echo "Canonical (${CANONICAL_DIR}):" >&2
echo "${CANONICAL_HASHES}" >&2
echo "" >&2
echo "Embedded (${EMBEDDED_DIR}):" >&2
echo "${EMBEDDED_HASHES}" >&2
echo "" >&2
echo "To sync the embedded copy from canonical:" >&2
echo "  cp -r ${CANONICAL_DIR}/* ${EMBEDDED_DIR}/" >&2
echo "  rm -rf ${EMBEDDED_DIR}/__pycache__" >&2
exit 1

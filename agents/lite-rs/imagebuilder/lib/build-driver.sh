#!/usr/bin/env bash
#
# imagebuilder/lib/build-driver.sh — orchestrator entry point.
#
# Usage:
#   build-driver.sh <board-slug>     run the recipe for a board
#   build-driver.sh --check          smoke-check the imagebuilder/ tree
#   build-driver.sh --list           list known boards
#
# Honours:
#   IMGBUILD_OUTPUT      output dir (default: $REPO_ROOT/output)
#   IMGBUILD_VERSION     image version tag (default: 0.1.0)
#   LITE_AGENT_MINISIGN_KEY       repo secret; signs the .img.gz
#   LITE_AGENT_MINISIGN_PASSWORD  repo secret; password for the key

set -eu

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=common.sh
. "${SCRIPT_DIR}/common.sh"

usage() {
    cat <<EOF
Usage: build-driver.sh <board-slug>
       build-driver.sh --check
       build-driver.sh --list

Build a flashable SD-card image for a single board. Output lands at:
  \${IMGBUILD_OUTPUT:-output}/<slug>/ados-<slug>-<version>.img.gz
        ditto                          /ados-<slug>-<version>.img.gz.sha256
        ditto                          /ados-<slug>-<version>.img.gz.minisig

Known boards:
EOF
    for d in "${IMGBUILD_ROOT}/boards"/*/; do
        [ -d "${d}" ] && printf '  - %s\n' "$(basename "${d}")"
    done
}

case "${1:-}" in
    -h|--help)
        usage
        exit 0
        ;;
    --check)
        imgbuild::check
        exit 0
        ;;
    --list)
        for d in "${IMGBUILD_ROOT}/boards"/*/; do
            [ -d "${d}" ] && basename "${d}"
        done
        exit 0
        ;;
    "")
        usage
        exit 1
        ;;
    *)
        slug="$1"
        if [ ! -d "${IMGBUILD_ROOT}/boards/${slug}" ]; then
            imgbuild::log_error "no recipe for board slug: ${slug}"
            usage
            exit 1
        fi
        imgbuild::run_recipe "${slug}"
        ;;
esac

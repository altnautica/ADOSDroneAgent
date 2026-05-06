#!/usr/bin/env bash
# =============================================================================
# Run the bats suite for the lightweight installer dispatcher.
#
# Wraps `bats` with a clear error message when the binary is not on
# PATH, so CI logs surface the right install hint instead of a bare
# "command not found".
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUITE="${SCRIPT_DIR}/install_dispatcher.bats"

if ! command -v bats >/dev/null 2>&1; then
    cat <<'EOF' >&2
bats is not installed.

Install via:
  apt-get install -y bats        (Debian/Ubuntu)
  apk add bats                   (Alpine)
  brew install bats-core         (macOS)

Or follow the upstream instructions at https://bats-core.readthedocs.io/.
EOF
    exit 127
fi

if [ ! -f "${SUITE}" ]; then
    echo "missing test file: ${SUITE}" >&2
    exit 1
fi

exec bats "${SUITE}"

#!/usr/bin/env bash
# =============================================================================
# Run the bats suites for the installer dispatchers.
#
# Covers the lightweight installer target dispatcher and the prebuilt
# kernel-module install path. Wraps `bats` with a clear error message when
# the binary is not on PATH, so CI logs surface the right install hint
# instead of a bare "command not found".
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUITES=(
    "${SCRIPT_DIR}/install_dispatcher.bats"
    "${SCRIPT_DIR}/prebuilt_dispatcher.bats"
)

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

for suite in "${SUITES[@]}"; do
    if [ ! -f "${suite}" ]; then
        echo "missing test file: ${suite}" >&2
        exit 1
    fi
done

exec bats "${SUITES[@]}"

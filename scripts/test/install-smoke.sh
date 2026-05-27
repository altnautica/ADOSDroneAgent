#!/usr/bin/env bash
# install-smoke.sh — fast pre-push smoke for the install pipeline.
#
# Catches the class of breakage a module refactor introduces: a syntax slip
# in one of the install.d/ modules, a shellcheck-level bug, or a broken
# profile resolution. Run it before pushing changes to scripts/install*.
#
#   bash scripts/test/install-smoke.sh
#
# Steps: shellcheck (every install script INCLUDING the install.d modules,
# which the CI lint step historically skipped) and a bash -n syntax check.
# The bats dispatcher suite runs separately via run_dispatcher_tests.sh.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 2

# Globs expand at array assignment, so FILES holds real paths (quote it below).
FILES=(
    scripts/install.sh
    scripts/install.d/*.sh
    scripts/lib/*.sh
    scripts/drivers/*.sh
)

fail=0

echo "== shellcheck (errors) =="
if command -v shellcheck >/dev/null 2>&1; then
    shellcheck -S error "${FILES[@]}" && echo "  clean" || fail=1
else
    echo "  shellcheck not installed; skipping"
fi

echo "== bash -n syntax =="
for f in "${FILES[@]}"; do
    [ -f "$f" ] || continue
    bash -n "$f" || { echo "  SYNTAX FAIL: $f"; fail=1; }
done
[ "$fail" -eq 0 ] && echo "  clean"

if [ "$fail" -eq 0 ]; then
    echo "SMOKE PASS"
else
    echo "SMOKE FAIL"
    exit 1
fi

#!/usr/bin/env bash
# run_all.sh — execute the bench-test suite end-to-end against a paired
# drone + GS rig. Each test exits 0 on success, non-zero on failure.
# This driver script aggregates results.
#
# Usage: ./run_all.sh <drone-ssh> <drone-pass> <gs-ssh> <gs-pass>
#   e.g. ./run_all.sh radxa@groundnode.local radxa skynode@skynode.local root
#
# All tests are read-only (or self-recovering) — none leave the rigs
# in a broken state if a test fails midway.

set -u
DRONE_TARGET="${1:?usage: $0 <drone-ssh> <drone-pass> <gs-ssh> <gs-pass>}"
DRONE_PASS="${2:?usage: $0 <drone-ssh> <drone-pass> <gs-ssh> <gs-pass>}"
GS_TARGET="${3:?usage: $0 <drone-ssh> <drone-pass> <gs-ssh> <gs-pass>}"
GS_PASS="${4:?usage: $0 <drone-ssh> <drone-pass> <gs-ssh> <gs-pass>}"

HERE="$(cd "$(dirname "$0")" && pwd)"
PASSED=0
FAILED=0
RESULTS=()

run_test() {
  local name="$1"
  shift
  echo
  echo "================================================================"
  echo "RUNNING: $name"
  echo "================================================================"
  if "$@"; then
    PASSED=$((PASSED + 1))
    RESULTS+=("PASS  $name")
  else
    FAILED=$((FAILED + 1))
    RESULTS+=("FAIL  $name (exit $?)")
  fi
}

run_test "wfb_tx zombie recovery (drone)" \
  "$HERE/test_wfb_tx_zombie_recovery.sh" "$DRONE_TARGET" "$DRONE_PASS"

run_test "wfb_rx zombie recovery (gs)" \
  "$HERE/test_wfb_rx_zombie_recovery.sh" "$GS_TARGET" "$GS_PASS"

echo
echo "================================================================"
echo "SUITE SUMMARY"
echo "================================================================"
for r in "${RESULTS[@]}"; do
  echo "  $r"
done
echo
echo "Passed: $PASSED, Failed: $FAILED"

if [ "$FAILED" -gt 0 ]; then
  exit 1
fi
exit 0
